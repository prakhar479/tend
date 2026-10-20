//! Controller: the closed loop. Sense → judge → decide → act → record.
//!
//! The controller is the only writer of ledger entries. It returns a `Decision`:
//! entries and blobs to persist, plus a `Response` for the connector to apply.
//! In look mode the response is always empty; actions are recorded with `applied: false`.

use std::collections::{BTreeMap, HashMap};

/// An edit, by path and content hashes. A blocked edit is allowed by this key.
type EditKey = (String, String, String);

use crate::broker::{Answer, Broker, Question};
use crate::capability::Capabilities;
use crate::event::{Event, Mode};
use crate::guard::{self, GuardDecision};
use crate::loops::{LoopDetector, LoopSignal};
use crate::perception::{
    classify, command_key, extract_failures, shell, writes_files, Failure, OutputKind,
};
use crate::record::{ActionKind, Body, Entry, SteerTrigger, LEDGER_FORMAT};
use crate::session::{Change, SessionModel};
use crate::shape::{shape, Shaped};
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
    /// Loop time counts at most this much of any gap between events (idle time).
    pub idle_gap_minutes: u64,
    /// `tend claude` deletes sessions with no activity for this many days (0: never).
    pub keep_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shape_threshold_tokens: 2000,
            max_summary_ratio: 0.7,
            loop_threshold: 3,
            tighten_after: 5,
            idle_gap_minutes: 10,
            keep_days: 30,
        }
    }
}

impl Config {
    /// Every knob as `name → value`, in a stable order.
    pub fn fields(&self) -> BTreeMap<String, String> {
        [
            ("idle_gap_minutes", self.idle_gap_minutes.to_string()),
            ("keep_days", self.keep_days.to_string()),
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
    /// Run this Bash call through `tend run` (PreToolUse input rewrite).
    pub wrap: bool,
    pub add_context: Vec<String>,
    pub permission: Option<Permission>,
    /// The agent may retry a denied call (R3.7).
    pub retry: bool,
}

impl Response {
    pub fn is_empty(&self) -> bool {
        self.replace_output.is_none()
            && !self.wrap
            && self.add_context.is_empty()
            && self.permission.is_none()
            && !self.retry
    }

    pub fn merge(&mut self, other: Response) {
        if other.replace_output.is_some() {
            self.replace_output = other.replace_output;
        }
        self.wrap |= other.wrap;
        self.retry |= other.retry;
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
    /// Blocked guard actions, by action id, and the most recent one.
    blocked: BTreeMap<String, EditKey>,
    last_blocked: Option<String>,
    /// Edits the user allowed once, with the action id and the override entry seq.
    allowed: BTreeMap<EditKey, (String, u64)>,
    /// Bash calls wrapped by `tend run`, by tool use id.
    wrapped: BTreeMap<String, Wrapped>,
    /// Command keys never to wrap again: a wrapped run of them got no tool result
    /// (Claude Code's auto-mode classifier can deny a rewrite as a bypass).
    no_wrap: std::collections::BTreeSet<String>,
    /// The last summary the agent saw, to spot an immediate re-run of its command.
    last_summary: Option<LastSummary>,
    /// Set by `observe` in deferred mode; `filter` moves it into the wrapped call.
    deferred: Option<PendingShape>,
    /// Active clock for loop time: wall time with idle gaps capped.
    last_ms: Option<u64>,
    active_ms: u64,
}

/// The costly, pure part of handling an output: hash, failures, and summary.
#[derive(Debug, Clone)]
pub struct Prepared {
    hash: String,
    lines: usize,
    tokens: usize,
    failures: Vec<Failure>,
    /// The summary at `level`, if the output is large enough and the summary pays off.
    shaped: Option<Shaped>,
    level: u8,
}

/// Prepare an output for `observe`. Pure, so the daemon can run it without its lock.
pub fn prepare(output: &str, level: u8, cfg: &Config) -> Prepared {
    let tokens = estimate_tokens(output);
    let shaped = if tokens >= cfg.shape_threshold_tokens {
        shape(output, level, cfg.max_summary_ratio)
    } else {
        None
    };
    Prepared {
        hash: sha256_hex(output),
        lines: output.lines().count(),
        tokens,
        failures: extract_failures(output),
        shaped,
        level,
    }
}

/// A simple command of `command` runs `tend allow` (any path to tend). Text inside
/// quotes, such as a commit message, does not count.
fn runs_tend_allow(command: &str) -> bool {
    shell::parse(command).segments.iter().any(|seg| {
        let w = shell::program_words(seg);
        w.len() >= 2 && shell::program_name(&w[0]) == "tend" && w[1] == "allow"
    })
}

/// Whether `observe` may put a summary in place.
#[derive(Debug, Clone, Copy)]
enum Replace {
    /// Decide now; the bool is whether the agent supports replacing this output.
    Now(bool),
    /// `tend run` prints it; record it once the tool result shows what the agent saw.
    Deferred,
}

#[derive(Debug, Clone)]
struct LastSummary {
    command: String,
    output_id: String,
    kind: OutputKind,
    /// Edit count when the summary was shown.
    edits: u64,
}

#[derive(Debug, Clone)]
struct Wrapped {
    /// The command the agent asked for.
    command: String,
    /// Seq of the wrap action (cause of the output entry).
    action_seq: u64,
    /// `tend run` reported the output.
    filtered: bool,
    /// A summary `tend run` was told to print. It is recorded once the tool result
    /// shows what the agent really saw (R0.2).
    pending: Option<PendingShape>,
}

#[derive(Debug, Clone)]
struct PendingShape {
    action: ActionKind,
    reason: String,
    text: String,
    kind: OutputKind,
    /// Seq of the output entry.
    out: u64,
    command: String,
    output_id: String,
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
            blocked: BTreeMap::new(),
            last_blocked: None,
            allowed: BTreeMap::new(),
            wrapped: BTreeMap::new(),
            no_wrap: std::collections::BTreeSet::new(),
            last_summary: None,
            deferred: None,
            last_ms: None,
            active_ms: 0,
        }
    }

    /// Advance the active clock to `now_ms`. A gap longer than `idle_gap_minutes`
    /// counts as that long, so a session left open overnight does not inflate loop time.
    fn tick(&mut self, now_ms: u64) -> u64 {
        let cap = self.cfg.idle_gap_minutes.saturating_mul(60_000);
        if let Some(last) = self.last_ms {
            self.active_ms += now_ms.saturating_sub(last).min(cap);
        }
        self.last_ms = Some(self.last_ms.map_or(now_ms, |l| l.max(now_ms)));
        self.active_ms
    }

    /// Handle an event whose reply would reach the agent too late (D9): as in look mode,
    /// so the ledger records no action as applied, and with no response.
    pub fn handle_unanswered(&mut self, event: Event, now_ms: u64) -> Decision {
        let mode = std::mem::replace(&mut self.session.mode, Mode::Look);
        let mut d = self.handle(event, now_ms);
        self.session.mode = mode;
        d.response = Response::default();
        d
    }

    /// The reply with these decisions' applied actions reached no one (D9). Record that,
    /// and drop state that assumed the agent got them.
    pub fn undelivered(&mut self, decisions: &[Decision], now_ms: u64) -> Decision {
        let mut out = Decision::default();
        let (mut ids, mut causes) = (Vec::new(), Vec::new());
        for e in decisions.iter().flat_map(|d| &d.entries) {
            if let Body::Action {
                id,
                action,
                applied: true,
                ..
            } = &e.body
            {
                ids.push(id.clone());
                causes.push(e.seq);
                if matches!(action, ActionKind::Shape { .. }) {
                    self.last_summary = None;
                }
            }
        }
        if ids.is_empty() {
            return out;
        }
        for id in &ids {
            self.blocked.remove(id);
            if self.last_blocked.as_ref() == Some(id) {
                self.last_blocked = None;
            }
        }
        // The agent runs the command as it wrote it.
        self.wrapped.retain(|_, w| !causes.contains(&w.action_seq));
        let reason =
            "The hook reply came after the shim's timeout, so the agent did not get these actions.";
        self.push(
            &mut out,
            now_ms,
            causes,
            Body::Undelivered {
                actions: ids,
                reason: reason.into(),
            },
        );
        out
    }

    /// The user allows one blocked edit (the latest one if `action` is None).
    /// Err is a message for the user.
    pub fn allow(&mut self, action: Option<&str>, now_ms: u64) -> Result<Decision, String> {
        let id = match action {
            Some(a) => a.to_string(),
            None => self
                .last_blocked
                .clone()
                .ok_or("No edit was blocked in this session.")?,
        };
        let key = self
            .blocked
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("{id} is not a blocked edit."))?;
        let mut d = Decision::default();
        let seq = self.push(
            &mut d,
            now_ms,
            vec![],
            Body::Override { action: id.clone() },
        );
        self.allowed.insert(key, (id, seq));
        Ok(d)
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
        self.tick(now_ms);
        let mut d = Decision::default();
        let is_bash = matches!(&event, Event::ToolCompleted { tool, .. } if tool == "Bash");
        match event {
            _ if is_bash => self.on_bash(&mut d, event, now_ms),
            Event::EditProposed { .. } => self.on_edit_proposed(&mut d, event, now_ms),
            Event::FileEdited { .. } => self.on_file_edited(&mut d, event, now_ms),
            Event::FileChanged { .. } => self.on_file_changed(&mut d, event, now_ms),
            Event::ToolDenied { .. } => self.on_tool_denied(&mut d, event, now_ms),
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
            Event::ToolRequested { .. } => self.on_tool_requested(&mut d, event, now_ms),
            Event::TurnEnded => {
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                self.end_turn(&mut d, now_ms, ev);
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
        // A wrapped call reports the original command; the hook sees the wrapper.
        let command = self
            .wrapped
            .get(&tool_use_id)
            .map(|w| w.command.clone())
            .unwrap_or(command);
        let info = classify(&command);
        let hash = sha256_hex(&output);
        let wrapped = self.wrapped.remove(&tool_use_id);
        let filtered = wrapped.as_ref().is_some_and(|w| w.filtered);
        let stored = Event::ToolCompleted {
            tool_use_id,
            tool,
            input,
            output: format!("<blob:{hash}>"),
            failed,
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        if filtered {
            // `tend run` already recorded the full output; this is what the agent saw.
            if let Some(p) = wrapped.and_then(|w| w.pending) {
                self.confirm_shape(d, now, ev, p, &output);
            }
            d.blobs.push(Blob {
                hash,
                content: output,
            });
            return;
        }

        // Re-open of a summary: the agent needed the full output.
        if let Some(target) = info.show_target.as_deref() {
            if let Some(kind) = self.outputs.get(target).copied() {
                if let Some(change) = self.session.on_reopen(kind, &format!("re-opened {target}")) {
                    self.adapt(d, now, ev, change);
                }
            }
            return;
        }

        let supported = if failed {
            self.caps.replace_failed_output
        } else {
            self.caps.replace_output
        };
        let mode = Replace::Now(supported);
        let p = prepare(&output, self.session.shape_level(info.kind), &self.cfg);
        if let Some(text) = self.observe(d, now, ev, &command, info.kind, output, p, mode) {
            d.response.replace_output = Some(text);
        }
    }

    /// Store an output, look for loops, and shape it. Returns the summary to show, if
    /// the shape action applies. `can_replace`: the caller can put the summary in place.
    #[allow(clippy::too_many_arguments)]
    fn observe(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        command: &str,
        kind: OutputKind,
        output: String,
        p: Prepared,
        can_replace: Replace,
    ) -> Option<String> {
        // A Bash write before the run in the same call (`python3 - <<EOF … EOF; pytest`)
        // is an edit for loop detection: agents edit mostly through Bash (R1.3).
        if writes_files(command) {
            self.loops.note_edit();
        }
        // Loops and re-runs compare normalized keys: agents vary pipes and flags (R1.3).
        let key = command_key(command);
        let command = key.as_str();
        // Re-running the same command right after a summary, with no edit in between,
        // means the summary hid something the agent wanted: treat it as a re-open.
        if let Some(last) = self.last_summary.take() {
            if last.command == command && self.loops.edits() == last.edits {
                let what = format!("re-ran the command behind {} with no edit", last.output_id);
                if let Some(change) = self.session.on_reopen(last.kind, &what) {
                    self.adapt(d, now, cause, change);
                }
            }
        }
        let output_id = format!("o-{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(output_id.clone(), kind);
        d.blobs.push(Blob {
            hash: p.hash.clone(),
            content: output,
        });
        let out = self.push(
            d,
            now,
            vec![cause],
            Body::Output {
                id: output_id.clone(),
                blob: p.hash,
                output_kind: kind,
                lines: p.lines,
                tokens: p.tokens,
            },
        );

        // Loops. All failures that reach the threshold in one run share one steer. Only
        // test and build runs count: a file view or log dump that contains failure text
        // is not a run that failed (checkpoint C2).
        let failures = if matches!(kind, OutputKind::Test | OutputKind::Build) {
            p.failures
        } else {
            Vec::new()
        };
        let mut looped: Vec<(String, String, u64, u64)> = Vec::new();
        let active = self.active_ms;
        for obs in self.loops.on_run(command, &failures, active) {
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
            // Both ends on the active clock (idle gaps capped).
            let first = looped.iter().map(|l| l.3).min().unwrap_or(active);
            let duration_ms = Some(active.saturating_sub(first));
            let text = if looped.len() == 1 {
                format!(
                    "tend: this failure is unchanged after {edits} fix attempts (edit, then run): {}. A different approach may be needed.",
                    summaries[0]
                )
            } else {
                format!(
                    "tend: {} failures are unchanged after {edits} fix attempts (edit, then run): {}. A different approach may be needed.",
                    looped.len(),
                    summaries.join("; ")
                )
            };
            let reason = format!(
                "Failure signature(s) {} recurred after {edits} fix attempts.",
                signatures.join(", ")
            );
            let trigger = SteerTrigger::Loop {
                signatures,
                duration_ms,
            };
            self.steer(d, now, out, trigger, text, reason);
        }

        // Shaping (computed by `prepare`, possibly outside the daemon's lock).
        let level = p.level;
        let s = p.shaped?;
        let text = format!(
            "{}\n[tend: {} lines → {} · full output: tend show {}]",
            s.text, s.from_lines, s.to_lines, output_id
        );
        let reason = format!(
            "{} output was {} tokens; the summary keeps every failure line (level {level}).",
            kind.label(),
            s.from_tokens
        );
        let action = ActionKind::Shape {
            output_id: output_id.clone(),
            from_tokens: s.from_tokens,
            to_tokens: estimate_tokens(&text),
            from_lines: s.from_lines,
            to_lines: s.to_lines,
        };
        let supported = match can_replace {
            Replace::Now(s) => s,
            Replace::Deferred => {
                self.deferred = Some(PendingShape {
                    action,
                    reason,
                    text: text.clone(),
                    kind,
                    out,
                    command: command.to_string(),
                    output_id,
                });
                return Some(text);
            }
        };
        if !self.act_if(d, now, out, action, reason, supported) {
            return None;
        }
        self.shown(d, now, out, command, output_id, kind);
        Some(text)
    }

    /// A summary reached the agent: adapt, and remember it for the re-run signal.
    fn shown(
        &mut self,
        d: &mut Decision,
        now: u64,
        out: u64,
        command: &str,
        output_id: String,
        kind: OutputKind,
    ) {
        if let Some(change) = self.session.on_shaped(kind) {
            self.adapt(d, now, out, change);
        }
        self.last_summary = Some(LastSummary {
            command: command.to_string(),
            output_id,
            kind,
            edits: self.loops.edits(),
        });
    }

    /// Record a deferred shape against the tool result the agent saw. If `tend run`
    /// printed the original (the daemon answered too late), the summary was not applied.
    fn confirm_shape(&mut self, d: &mut Decision, now: u64, ev: u64, p: PendingShape, seen: &str) {
        if seen.trim_end().ends_with(p.text.trim_end()) {
            self.record(d, now, vec![p.out, ev], p.action, p.reason, true, false);
            self.shown(d, now, p.out, &p.command, p.output_id, p.kind);
        } else {
            let reason =
                "tend run printed the original output, so the agent did not see this summary.";
            self.record(
                d,
                now,
                vec![p.out, ev],
                p.action,
                reason.into(),
                false,
                false,
            );
        }
    }

    /// A turn ended. A wrapped call with no tool result was stopped: its summary was not
    /// seen. Drop every wrapped entry, so the map cannot grow (R0.7).
    fn end_turn(&mut self, d: &mut Decision, now: u64, ev: u64) {
        for (_, w) in std::mem::take(&mut self.wrapped) {
            // No tool result: denied (auto mode may call a rewrite a bypass) or stopped.
            // One strike: run this command unwrapped from now on.
            if !w.filtered {
                self.no_wrap.insert(command_key(&w.command));
            }
            if let Some(p) = w.pending {
                let reason = "No tool result arrived (the call was stopped), so the agent did not see this summary.";
                self.record(
                    d,
                    now,
                    vec![p.out, ev],
                    p.action,
                    reason.into(),
                    false,
                    false,
                );
            }
        }
    }

    /// `tend run` ran a wrapped command and asks for the text to print.
    /// Returns None when this call was not wrapped: print the original output.
    pub fn filter(
        &mut self,
        tool_use_id: &str,
        output: String,
        now_ms: u64,
    ) -> (Decision, Option<String>) {
        let Some(level) = self.filter_level(tool_use_id) else {
            return (Decision::default(), None);
        };
        let p = prepare(&output, level, &self.cfg);
        self.filter_prepared(tool_use_id, output, p, now_ms)
    }

    /// The shape level for a pending `tend run` report, or None if there is nothing to
    /// filter. With `prepare`, the daemon does the heavy work without holding its lock.
    pub fn filter_level(&self, tool_use_id: &str) -> Option<u8> {
        let w = self.wrapped.get(tool_use_id).filter(|w| !w.filtered)?;
        Some(self.session.shape_level(classify(&w.command).kind))
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// `filter` with the output already prepared.
    pub fn filter_prepared(
        &mut self,
        tool_use_id: &str,
        output: String,
        p: Prepared,
        now_ms: u64,
    ) -> (Decision, Option<String>) {
        self.tick(now_ms);
        let mut d = Decision::default();
        let Some(w) = self.wrapped.get_mut(tool_use_id) else {
            return (d, None);
        };
        if w.filtered {
            return (d, None);
        }
        w.filtered = true;
        let (command, cause) = (w.command.clone(), w.action_seq);
        let kind = classify(&command).kind;
        let text = self.observe(
            &mut d,
            now_ms,
            cause,
            &command,
            kind,
            output,
            p,
            Replace::Deferred,
        );
        if let Some(w) = self.wrapped.get_mut(tool_use_id) {
            w.pending = self.deferred.take();
        }
        (d, text)
    }

    /// Only the user may lift a guard. Block the agent's own `tend allow`.
    /// The match is on the command text, so a determined agent can get around it;
    /// the guard is advisory and every allow is in the ledger.
    fn on_tool_requested(&mut self, d: &mut Decision, event: Event, now: u64) {
        let (command, tool_use_id, rewrite_ok) = match &event {
            Event::ToolRequested {
                tool,
                input,
                tool_use_id,
                rewrite_ok,
            } if tool == "Bash" => (
                input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                tool_use_id.clone(),
                *rewrite_ok,
            ),
            _ => (String::new(), String::new(), false),
        };
        let ev = self.push(d, now, vec![], Body::Event { event });
        if !runs_tend_allow(&command) {
            self.maybe_wrap(d, now, ev, tool_use_id, command, rewrite_ok);
            return;
        }
        let message = "tend: only the user can allow a blocked edit. Ask the user to run `! tend allow`, then retry the edit.".to_string();
        let action = ActionKind::BlockCommand { command };
        if self.act(d, now, ev, action, message.clone()) {
            d.response.permission = Some(Permission::Deny(message));
        }
    }

    /// Failed runs cannot be shaped after the fact (S2). Run test commands through
    /// `tend run`, which asks for the summary before the agent sees the output.
    fn maybe_wrap(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        tool_use_id: String,
        command: String,
        rewrite_ok: bool,
    ) {
        let info = classify(&command);
        if info.kind != OutputKind::Test || info.show_target.is_some() || tool_use_id.is_empty() {
            return;
        }
        // `tend run` is a subprocess. Claude Code keeps only the working directory between
        // Bash calls (environment, aliases, and options do not persist), so only cd/pushd/
        // popd would be lost; `source venv/bin/activate && …` is safe to wrap (R1.1).
        // `bash -c` runs here-docs, subshells, and multi-line scripts as the agent's shell
        // would. The parser splits every line and `{ …; }` group, so a cd anywhere is
        // found (R0.5); one inside `( … )` or a here-doc body also refuses (conservative).
        // A leading `cd DIR &&` stays outside the wrapper (see shell::leading_cd).
        let (prefix, _) = shell::leading_cd(&command);
        let line = shell::parse(&command[prefix..]);
        let changes_state = line.segments.iter().any(|seg| {
            matches!(
                seg.first().map(String::as_str),
                Some("cd" | "pushd" | "popd")
            )
        });
        if changes_state {
            return;
        }
        if self.no_wrap.contains(&command_key(&command)) {
            let reason = "Not wrapped: a wrapped run of this command got no result before (denied or stopped).";
            let action = ActionKind::Wrap {
                tool_use_id: tool_use_id.clone(),
            };
            self.record(d, now, vec![cause], action, reason.into(), false, false);
            return;
        }
        let reason = if rewrite_ok {
            "Test command wrapped in `tend run`, so its output can be summarized even when it fails.".to_string()
        } else {
            "Not wrapped: a rewrite could add a permission prompt in this permission mode."
                .to_string()
        };
        let action = ActionKind::Wrap {
            tool_use_id: tool_use_id.clone(),
        };
        let supported = self.caps.rewrite_input && rewrite_ok;
        let seq = self.next_seq;
        if self.act_if(d, now, cause, action, reason, supported) {
            d.response.wrap = true;
            self.wrapped.insert(
                tool_use_id,
                Wrapped {
                    command,
                    action_seq: seq,
                    filtered: false,
                    pending: None,
                },
            );
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
        let key: EditKey = (path.clone(), old_hash.clone(), new_hash.clone());
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
        let Some((j, reasons)) = self.judge_test_edit(d, now, ev, &path, &old_text, &new_text)
        else {
            return;
        };

        let decision = self.effective_guard(self.session.guard);
        if let Some((id, override_seq)) = self.allowed.remove(&key) {
            let reason = format!("Allowed once by the user (`tend allow {id}`).");
            let action = ActionKind::Guard { path, decision };
            let a = self.next_action_id();
            self.push(
                d,
                now,
                vec![j, override_seq],
                Body::Action {
                    id: a,
                    action,
                    reason,
                    applied: false,
                    unsupported: false,
                },
            );
            return;
        }
        let blocking = decision != GuardDecision::Warn;
        let message = if blocking {
            format!(
                "tend: this edit may weaken tests in {path}: {}. If the user wants it anyway, they can run `! tend allow` and you can retry the same edit.",
                reasons.join("; ")
            )
        } else {
            format!(
                "tend: this edit may weaken tests in {path}: {}.",
                reasons.join("; ")
            )
        };
        let action = ActionKind::Guard { path, decision };
        let id = format!("a-{}", self.next_action);
        if self.act(d, now, j, action, message.clone()) {
            if blocking {
                self.blocked.insert(id.clone(), key);
                self.last_blocked = Some(id);
            }
            match decision {
                GuardDecision::Ask => d.response.permission = Some(Permission::Ask(message)),
                GuardDecision::Deny => d.response.permission = Some(Permission::Deny(message)),
                GuardDecision::Warn => d.response.add_context.push(message),
            }
        }
    }

    /// Ask whether an edit to a test file weakens it. Returns the judgment's seq and the
    /// reasons when it does. Files that are not tests are not judged.
    fn judge_test_edit(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        path: &str,
        old: &str,
        new: &str,
    ) -> Option<(u64, Vec<String>)> {
        if !guard::judges(path, old, new) {
            return None;
        }
        let q = Question::TestWeakened { path, old, new };
        let (answer, source) = self.broker.ask(&q);
        let (name, version) = (q.name(), q.version());
        let j = self.push(
            d,
            now,
            vec![cause],
            Body::Judgment {
                question: name.into(),
                version,
                subject: path.to_string(),
                answer: answer.clone(),
                source,
            },
        );
        match answer {
            Answer::Weakened { reasons } => Some((j, reasons)),
            _ => None,
        }
    }

    /// A shell command changed a test file (R1.4). The change already happened, so the
    /// guard can only warn: the agent hears it in context, and look mode records it.
    fn on_file_changed(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::FileChanged {
            tool_use_id,
            path,
            old_text,
            new_text,
        } = event
        else {
            return;
        };
        let (old_hash, new_hash) = (sha256_hex(&old_text), sha256_hex(&new_text));
        let stored = Event::FileChanged {
            tool_use_id,
            path: path.clone(),
            old_text: format!("<blob:{old_hash}>"),
            new_text: format!("<blob:{new_hash}>"),
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        d.blobs.push(Blob {
            hash: old_hash,
            content: old_text.clone(),
        });
        d.blobs.push(Blob {
            hash: new_hash.clone(),
            content: new_text.clone(),
        });
        // Edit, revert, edit through the shell is a cycle too.
        if let Some(LoopSignal::Cycle { path }) = self.loops.on_edit(&path, &new_hash) {
            let text = format!("tend: {path} is back to an earlier version (edit, revert, edit).");
            let reason = format!("Content hash of {path} repeated an earlier state.");
            self.steer(d, now, ev, SteerTrigger::Cycle { path }, text, reason);
        }
        let Some((j, reasons)) = self.judge_test_edit(d, now, ev, &path, &old_text, &new_text)
        else {
            return;
        };
        let message = format!(
            "tend: the last command changed {path} and may have weakened tests: {}. If that was not intended, restore the file.",
            reasons.join("; ")
        );
        let action = ActionKind::Guard {
            path,
            decision: GuardDecision::Warn,
        };
        if self.act(d, now, j, action, message.clone()) {
            d.response.add_context.push(message);
        }
    }

    /// Auto mode denied a call. If tend had wrapped it, the wrapper may be why: its
    /// classifier sometimes calls `tend run` a bypass (R3.7). Stop wrapping the command,
    /// and tell the agent it may retry; the retry runs unwrapped.
    fn on_tool_denied(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::ToolDenied { tool_use_id, .. } = &event else {
            return;
        };
        let tool_use_id = tool_use_id.clone();
        let ev = self.push(d, now, vec![], Body::Event { event });
        let Some(w) = self.wrapped.remove(&tool_use_id) else {
            return;
        };
        self.no_wrap.insert(command_key(&w.command));
        let reason = "Auto mode denied a call that tend had wrapped in `tend run`; the agent may retry it unwrapped.";
        let action = ActionKind::Retry { tool_use_id };
        let supported = self.supports(&action);
        let applied = self.session.mode == Mode::Optimize && supported;
        self.record(
            d,
            now,
            vec![ev, w.action_seq],
            action,
            reason.into(),
            applied,
            !supported,
        );
        d.response.retry = applied;
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
            let text = format!("tend: {path} is back to an earlier version (edit, revert, edit).");
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

    fn next_action_id(&mut self) -> String {
        let id = format!("a-{}", self.next_action);
        self.next_action += 1;
        id
    }

    fn supports(&self, action: &ActionKind) -> bool {
        match action {
            ActionKind::BlockCommand { .. } => self.caps.deny,
            ActionKind::Wrap { .. } => self.caps.rewrite_input,
            ActionKind::Retry { .. } => self.caps.retry_denied,
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
        self.record(d, now, vec![cause], action, reason, applied, !supported);
        applied
    }

    /// Push an action entry. Returns its seq.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        d: &mut Decision,
        now: u64,
        causes: Vec<u64>,
        action: ActionKind,
        reason: String,
        applied: bool,
        unsupported: bool,
    ) -> u64 {
        let id = self.next_action_id();
        self.push(
            d,
            now,
            causes,
            Body::Action {
                id,
                action,
                reason,
                applied,
                unsupported,
            },
        )
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
        assert!(text.contains("tend show o-1"));
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

    /// A large file view is never shaped: the agent asked for that content.
    #[test]
    fn file_views_are_not_shaped() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(bash("cat tests/test_api_output.txt", PYTEST), 0);
        assert_eq!(d.response.replace_output, None);
        assert!(!d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Shape { .. },
                ..
            }
        )));
    }

    #[test]
    fn reopen_loosens_shape_level() {
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest", PYTEST), 0);
        let d = c.handle(bash("tend show o-1", PYTEST), 1);
        assert!(d.response.is_empty(), "never shape a re-open");
        assert!(d
            .entries
            .iter()
            .any(|e| matches!(&e.body, Body::Adaptation { .. })));
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);
    }

    /// R1.3: the agent varies the pipe and flags; it is still the same test run.
    #[test]
    fn loops_and_reruns_use_normalized_commands() {
        let mut c = ctl(Mode::Optimize);
        let variants = [
            "pytest tests/test_api.py -v 2>&1 | tail -40",
            "python -m pytest tests/test_api.py -x --tb=short",
            "source .venv/bin/activate && pytest tests/test_api.py -vv | head -80",
            "pytest -v tests/test_api.py 2>&1",
        ];
        let mut steered = false;
        for (i, cmd) in variants.iter().enumerate() {
            if i > 0 {
                c.handle(edited("src/app.py", &format!("h{i}")), i as u64);
            }
            let d = c.handle(bash(cmd, PYTEST), i as u64);
            steered |= d.entries.iter().any(|e| {
                matches!(
                    &e.body,
                    Body::Action {
                        action: ActionKind::Steer { .. },
                        ..
                    }
                )
            });
        }
        assert!(steered, "one loop across command variants");

        // Asking for more of the same output right after a summary is a re-open.
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest -v | tail -20", PYTEST), 0);
        c.handle(bash("pytest -v | tail -200", PYTEST), 1);
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);
    }

    /// R1.3: Bash writes are fix attempts too.
    #[test]
    fn loops_count_bash_writes() {
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest tests/test_api.py -q", PYTEST), 0);
        let mut steered = false;
        for i in 1..=3u64 {
            c.handle(bash(&format!("sed -i 's/{i}/x/' src/app.py"), ""), i);
            let d = c.handle(bash("pytest tests/test_api.py -q | tail -30", PYTEST), i);
            steered |= d.entries.iter().any(|e| {
                matches!(
                    &e.body,
                    Body::Action {
                        action: ActionKind::Steer { .. },
                        ..
                    }
                )
            });
        }
        assert!(steered);
    }

    #[test]
    fn immediate_rerun_loosens_like_a_reopen() {
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest -v", PYTEST), 0);
        let d = c.handle(bash("pytest -v", PYTEST), 1);
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Adaptation { reason, .. } if reason.contains("re-ran the command behind o-1")
        )));
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);

        // After an edit, a re-run is a normal retry, not a re-open.
        let mut c = ctl(Mode::Optimize);
        c.handle(bash("pytest -v", PYTEST), 0);
        c.handle(edited("src/app.py", "h1"), 1);
        c.handle(bash("pytest -v", PYTEST), 2);
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL);

        // In look mode the agent saw the full output: nothing to re-open.
        let mut c = ctl(Mode::Look);
        c.handle(bash("pytest -v", PYTEST), 0);
        c.handle(bash("pytest -v", PYTEST), 1);
        assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL);
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
                .any(|t| t.contains("unchanged after 3 fix attempts"));
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

    /// I3, L1 and L2: a passing run that logs an error, re-run through `tee`, is no loop.
    #[test]
    fn logged_error_in_a_passing_run_is_not_a_loop() {
        let out = "ConnectionError: refused (retrying)\n3 passed in 0.12s\n";
        let mut c = ctl(Mode::Optimize);
        for i in 0..5 {
            c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
            let d = c.handle(bash("pytest -s tests/test_retry.py", out), i * 60_000);
            assert!(d.response.add_context.is_empty(), "run {i}");
        }
        let mut c = ctl(Mode::Optimize);
        for i in 0..5 {
            let d = c.handle(bash("pytest -q 2>&1 | tee /tmp/t.log", PYTEST), i * 60_000);
            assert!(d.response.add_context.is_empty(), "tee run {i}");
        }
    }

    /// Checkpoint C2: reading a log or test file that shows a failure is not a run.
    #[test]
    fn viewing_failure_text_is_not_a_loop() {
        let mut c = ctl(Mode::Optimize);
        for i in 0..=4 {
            c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
            let d = c.handle(bash("cat test-output.log", PYTEST), i * 60_000);
            assert!(d.response.add_context.is_empty(), "view {i}");
        }
    }

    /// Checkpoint C1: a large code read is never summarized, however it is spelled.
    #[test]
    fn large_file_reads_are_not_shaped() {
        let code: String = (0..3000)
            .map(|i| format!("    let value_{i} = compute(input, {i}); // step {i}\n"))
            .collect();
        let mut c = ctl(Mode::Optimize);
        for cmd in [
            "cd /app; cat -n src/lib.rs",
            "wc -l src/lib.rs && sed -n 1,3000p src/lib.rs",
            "git diff HEAD -- src/lib.rs",
            "grep -n let src/lib.rs",
        ] {
            let d = c.handle(bash(cmd, &code), 0);
            assert!(d.response.replace_output.is_none(), "{cmd}");
        }
    }

    fn weakening() -> Event {
        Event::EditProposed {
            tool_use_id: "x".into(),
            path: "tests/test_api.py".into(),
            old_text: "assert a == 1\nassert b == 2\n".into(),
            new_text: "assert a == 1\n".into(),
        }
    }

    fn bash_changed(path: &str, old: &str, new: &str) -> Event {
        Event::FileChanged {
            tool_use_id: "t1".into(),
            path: path.into(),
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    /// R1.4: a Bash write already happened, so the guard warns in context.
    #[test]
    fn guard_warns_after_a_bash_write_weakens_a_test() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(
            bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
            0,
        );
        assert!(d.response.permission.is_none());
        let [m] = &d.response.add_context[..] else {
            panic!("{:?}", d.response)
        };
        assert!(
            m.contains("the last command changed /r/tests/test_api.py"),
            "{m}"
        );
        assert!(m.contains("assertions removed (2 → 1)"), "{m}");
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Guard {
                    decision: GuardDecision::Warn,
                    ..
                },
                applied: true,
                ..
            }
        )));
        // The ledger keeps blob references, not the file text.
        assert!(
            d.entries.iter().any(|e| matches!(&e.body, Body::Event { event: Event::FileChanged { old_text, .. } } if old_text.starts_with("<blob:")))
        );

        // Look mode records the same action and changes nothing.
        let mut c = ctl(Mode::Look);
        let d = c.handle(
            bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
            0,
        );
        assert!(d.response.is_empty());
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Guard { .. },
                applied: false,
                ..
            }
        )));

        // A new test file, or a change outside tests, is not flagged.
        let d = c.handle(bash_changed("/r/tests/test_new.py", "", "assert a\n"), 1);
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Action { .. })));
        let d = c.handle(bash_changed("/r/src/app.py", "assert a\n", ""), 2);
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Judgment { .. })));
    }

    /// D9: a hook already past its deadline is handled as look mode: nothing applied,
    /// nothing returned, nothing to allow later.
    #[test]
    fn late_hook_records_nothing_as_applied() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle_unanswered(weakening(), 0);
        assert!(d.response.is_empty());
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Guard { .. },
                applied: false,
                ..
            }
        )));
        assert!(c.allow(None, 1).is_err(), "no block reached the agent");
        assert_eq!(c.mode(), Mode::Optimize, "mode is restored");
        assert!(c.handle(weakening(), 2).response.permission.is_some());
    }

    /// D9: a reply that missed its deadline after handling is recorded as undelivered,
    /// and state that assumed the agent got it is dropped.
    #[test]
    fn undelivered_reply_is_recorded_and_forgotten() {
        let mut c = ctl(Mode::Optimize);
        let deny = c.handle(weakening(), 0);
        let wrap = c.handle(requested("t1", "pytest -q", true), 1);
        assert!(wrap.response.wrap);
        let d = c.undelivered(&[deny, wrap], 2);
        assert!(matches!(
            &d.entries[..],
            [Entry { body: Body::Undelivered { actions, .. }, .. }] if actions.len() == 2
        ));
        assert!(
            c.allow(None, 3).is_err(),
            "the deny never reached the agent"
        );
        assert_eq!(c.filter_level("t1"), None, "the call runs unwrapped");
        assert!(c.undelivered(&[], 4).entries.is_empty());
    }

    #[test]
    fn guard_denies_with_reason_in_claude_code() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(weakening(), 0);
        match d.response.permission {
            Some(Permission::Deny(m)) => {
                assert!(m.contains("assertions removed (2 → 1)"), "{m}");
                assert!(m.contains("! tend allow"), "{m}");
            }
            p => panic!("want deny, got {p:?}"),
        }
    }

    #[test]
    fn user_allows_a_blocked_edit_once() {
        let mut c = ctl(Mode::Optimize);
        c.handle(weakening(), 0);
        assert!(c.allow(Some("a-9"), 1).is_err(), "unknown action");
        let d = c.allow(None, 1).expect("allow the latest");
        assert!(matches!(&d.entries[0].body, Body::Override { action } if action == "a-1"));
        let override_seq = d.entries[0].seq;

        let d = c.handle(weakening(), 2);
        assert_eq!(d.response.permission, None, "allowed once");
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action { applied: false, reason, .. } if reason.contains("tend allow a-1")
        ) && e.causes.contains(&override_seq)));

        let d = c.handle(weakening(), 3);
        assert!(
            matches!(d.response.permission, Some(Permission::Deny(_))),
            "only once"
        );
    }

    /// G6: an agent that cannot deny gets a warning, and a warning is nothing to allow.
    #[test]
    fn guard_warns_where_the_agent_cannot_deny() {
        let caps = Capabilities {
            deny: false,
            ask: false,
            ..Capabilities::CLAUDE_CODE
        };
        let mut c = Controller::new(Config::default(), caps, Mode::Optimize);
        let d = c.handle(weakening(), 0);
        assert_eq!(d.response.permission, None);
        assert!(d
            .response
            .add_context
            .iter()
            .any(|m| m.contains("may weaken tests")));
        assert!(!d.response.add_context[0].contains("tend allow"));
        assert!(c.allow(None, 1).is_err(), "a warning blocks nothing");
    }

    /// G6: the Bash-write warning (R1.4) is not a block either.
    #[test]
    fn bash_write_warning_is_nothing_to_allow() {
        let mut c = ctl(Mode::Optimize);
        c.handle(
            bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
            0,
        );
        assert!(c.allow(None, 1).is_err());
    }

    /// G2: an edit of a Rust unit test in `src/` is judged.
    #[test]
    fn inline_rust_test_edit_is_guarded() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(
            Event::EditProposed {
                tool_use_id: "x".into(),
                path: "crates/shop/src/orders.rs".into(),
                old_text: "#[test]\nfn total() {\n    assert_eq!(total(), 6);\n}\n".into(),
                new_text: "#[test]\n#[ignore]\nfn total() {\n    assert_eq!(total(), 6);\n}\n"
                    .into(),
            },
            0,
        );
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
    }

    fn requested(id: &str, cmd: &str, rewrite_ok: bool) -> Event {
        Event::ToolRequested {
            tool_use_id: id.into(),
            tool: "Bash".into(),
            input: json!({ "command": cmd }),
            rewrite_ok,
        }
    }

    fn failed_bash(id: &str, cmd: &str, out: &str) -> Event {
        Event::ToolCompleted {
            tool_use_id: id.into(),
            tool: "Bash".into(),
            input: json!({ "command": cmd }),
            output: out.into(),
            failed: true,
        }
    }

    #[test]
    fn failing_test_run_is_shaped_through_tend_run() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(requested("t1", "pytest -v", true), 0);
        assert!(d.response.wrap, "test command is wrapped");

        // `tend run` reports the full output and prints the summary.
        let (d, text) = c.filter("t1", PYTEST.into(), 1);
        let text = text.expect("summary");
        assert!(text.contains("tend show o-1"));
        assert!(text.contains("FAILED tests/test_api.py::test_get_order"));
        assert!(
            !d.entries.iter().any(|e| matches!(
                e.body,
                Body::Action {
                    action: ActionKind::Shape { .. },
                    ..
                }
            )),
            "not recorded until the agent's view is known"
        );
        assert_eq!(c.filter("t1", PYTEST.into(), 1).1, None, "only once");

        // The hook sees the wrapper and the summary: record the shape, do not re-shape.
        let seen = format!("Exit code 1\n{text}");
        let d = c.handle(
            failed_bash("t1", "tend run --id t1 --sh 'pytest -v'", &seen),
            2,
        );
        assert!(d.response.is_empty());
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Output { .. })));
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                applied: true,
                action: ActionKind::Shape { .. },
                ..
            }
        )));
        assert_eq!(d.blobs.len(), 1, "the summary the agent saw is stored");
    }

    /// R0.2 (D3): the daemon answered too late, so tend run printed the original.
    #[test]
    fn late_filter_reply_is_recorded_as_not_applied() {
        let mut c = ctl(Mode::Optimize);
        c.handle(requested("t1", "pytest -v", true), 0);
        c.filter("t1", PYTEST.into(), 1).1.expect("summary");
        let seen = format!("Exit code 1\n{PYTEST}");
        let d = c.handle(
            failed_bash("t1", "tend run --id t1 --sh 'pytest -v'", &seen),
            2,
        );
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action { applied: false, action: ActionKind::Shape { .. }, reason, .. }
                if reason.contains("printed the original")
        )));
        // No summary was seen, so running it again is not a re-open.
        c.handle(requested("t2", "pytest -v", true), 3);
        let (d, _) = c.filter("t2", PYTEST.into(), 4);
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Adaptation { .. })));
    }

    /// Auto mode denied a wrapped call ([Auto-Mode Bypass]): no tool result, no filter.
    /// The retry must run as the agent wrote it.
    #[test]
    fn denied_wrap_is_not_repeated() {
        let mut c = ctl(Mode::Optimize);
        assert!(
            c.handle(requested("t1", "pytest -q tests/a.py", true), 0)
                .response
                .wrap
        );
        c.handle(Event::TurnEnded, 1);
        let d = c.handle(requested("t2", "pytest -v tests/a.py | tail", true), 2);
        assert!(!d.response.wrap, "same command key: not wrapped again");
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action { applied: false, action: ActionKind::Wrap { .. }, reason, .. }
                if reason.contains("got no result before")
        )));
        assert!(
            c.handle(requested("t3", "pytest tests/b.py", true), 3)
                .response
                .wrap
        );
    }

    fn denied(id: &str) -> Event {
        Event::ToolDenied {
            tool_use_id: id.into(),
            tool: "Bash".into(),
            reason: "[Auto-Mode Bypass] wraps the command".into(),
        }
    }

    /// R3.7: auto mode denied a wrapped call. The agent may retry, and the retry runs
    /// unwrapped at once (not only after the turn ends).
    #[test]
    fn denied_wrapped_call_is_retried_unwrapped() {
        let mut c = ctl(Mode::Optimize);
        assert!(
            c.handle(requested("t1", "pytest -q", true), 0)
                .response
                .wrap
        );
        let d = c.handle(denied("t1"), 1);
        assert!(d.response.retry);
        let retry = d.entries.iter().find(|e| {
            matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Retry { .. },
                    applied: true,
                    ..
                }
            )
        });
        assert_eq!(
            retry.map(|e| e.causes.len()),
            Some(2),
            "the denial and the wrap"
        );
        assert!(
            !c.handle(requested("t2", "pytest -q", true), 2)
                .response
                .wrap
        );

        // A denial of a call tend did not change is only recorded.
        let d = c.handle(denied("t9"), 3);
        assert!(d.response.is_empty());
        assert!(!d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Action { .. })));
    }

    /// R0.2/R0.7: a stopped call never gets a tool result; the turn end settles it.
    #[test]
    fn stopped_wrapped_call_is_settled_at_turn_end() {
        let mut c = ctl(Mode::Optimize);
        c.handle(requested("t1", "pytest -v", true), 0);
        c.filter("t1", PYTEST.into(), 1);
        c.handle(requested("t2", "pytest -q", true), 2);
        let d = c.handle(Event::TurnEnded, 3);
        let settled = d
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    &e.body,
                    Body::Action { applied: false, action: ActionKind::Shape { .. }, reason, .. }
                        if reason.contains("No tool result arrived")
                )
            })
            .count();
        assert_eq!(settled, 1, "only the call that had a summary");
        assert_eq!(c.filter("t2", PYTEST.into(), 4).1, None, "entries dropped");
    }

    #[test]
    fn loops_are_found_through_tend_run() {
        let mut c = ctl(Mode::Optimize);
        let mut steered = false;
        for i in 0..4u64 {
            if i > 0 {
                c.handle(edited("src/app.py", &format!("h{i}")), i);
            }
            let id = format!("t{i}");
            c.handle(requested(&id, "pytest -v", true), i);
            let (d, _) = c.filter(&id, PYTEST.into(), i);
            steered |= d.entries.iter().any(|e| {
                matches!(
                    &e.body,
                    Body::Action {
                        action: ActionKind::Steer { .. },
                        ..
                    }
                )
            });
        }
        assert!(steered, "original command keys the loop detector");
    }

    #[test]
    fn wrap_downgrades_honestly() {
        let mut c = ctl(Mode::Optimize);
        let d = c.handle(requested("a", "pytest", false), 0);
        assert!(!d.response.wrap, "permission mode may prompt");
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                unsupported: true,
                action: ActionKind::Wrap { .. },
                ..
            }
        )));
        // R1.1: a leading cd stays in the agent's shell, so the rest can be wrapped;
        // a cd after the test would be lost.
        assert!(
            c.handle(requested("b", "cd api && pytest", true), 1)
                .response
                .wrap
        );
        assert!(
            !c.handle(requested("b1", "pytest && cd api", true), 1)
                .response
                .wrap
        );
        // R1.1: state that does not outlive the call is fine to wrap.
        for (id, cmd) in [
            (
                "w1",
                "source .venv/bin/activate && python -m pytest -q 2>&1 | tail -20",
            ),
            ("w2", "export CI=1 && pytest"),
            ("w3", "set -e; CI=1 pytest -x"),
            (
                "w4",
                "cd /home/u/app && source .venv/bin/activate && pytest -q",
            ),
        ] {
            assert!(c.handle(requested(id, cmd, true), 1).response.wrap, "{cmd}");
        }
        // R0.5: newline and group forms of a state change are caught too.
        for (id, cmd) in [
            ("b2", "cd api\npytest"),
            ("b3", "{ cd api; pytest; }"),
            ("b4", "git commit -m 'fix pytest'"),
        ] {
            assert!(
                !c.handle(requested(id, cmd, true), 1).response.wrap,
                "{cmd}"
            );
        }
        assert!(!c.handle(requested("c", "ls -la", true), 2).response.wrap);
        assert!(
            !ctl(Mode::Look)
                .handle(requested("d", "pytest", true), 0)
                .response
                .wrap
        );
        // Unwrapped or unknown ids: print the original.
        assert_eq!(c.filter("zzz", "out".into(), 3).1, None);
        // A wrapped call whose `tend run` never reported falls back to normal handling.
        c.handle(requested("e", "pytest -v", true), 4);
        let d = c.handle(
            failed_bash("e", "tend run --id e --sh 'pytest -v'", PYTEST),
            5,
        );
        assert!(d
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Output { .. })));
    }

    #[test]
    fn agent_cannot_allow_itself() {
        let mut c = ctl(Mode::Optimize);
        let req = |cmd: &str| requested("b", cmd, false);
        let d = c.handle(req("cd /x && tend allow a-1"), 0);
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
        let d = c.handle(req("/home/u/.cargo/bin/tend allow"), 0);
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
        // R0.8: a commit message that mentions it is not running it.
        let d = c.handle(req("git commit -m 'document tend allow'"), 1);
        assert!(d.response.is_empty());
        assert!(c.handle(req("tend show o-1"), 1).response.is_empty());
        assert!(ctl(Mode::Look)
            .handle(req("tend allow"), 0)
            .response
            .is_empty());
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
    fn loop_time_skips_idle_gaps() {
        let mut c = ctl(Mode::Optimize);
        let min = 60_000;
        c.handle(bash("pytest", PYTEST), 0);
        // Edit 1 a minute later; then the session sits idle for two days.
        c.handle(edited("src/app.py", "h1"), min);
        c.handle(bash("pytest", PYTEST), 2 * min);
        let back = 2 * 24 * 60 * min;
        c.handle(edited("src/app.py", "h2"), back);
        c.handle(bash("pytest", PYTEST), back + min);
        c.handle(edited("src/app.py", "h3"), back + 2 * min);
        let d = c.handle(bash("pytest", PYTEST), back + 3 * min);
        let dur = d.entries.iter().find_map(|e| match &e.body {
            Body::Action {
                action:
                    ActionKind::Steer {
                        trigger: SteerTrigger::Loop { duration_ms, .. },
                        ..
                    },
                ..
            } => *duration_ms,
            _ => None,
        });
        // 2 min + 10 min (capped idle gap) + 3 min.
        assert_eq!(dur, Some(15 * min));
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
