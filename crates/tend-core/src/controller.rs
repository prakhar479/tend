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

/// Largest edit text (old or new) kept as a blob.
const MAX_EDIT_BLOB_BYTES: usize = 1 << 20;

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
    /// `tend claude` audits what the session changed in tests and checks (experimental).
    pub experimental_audit: bool,
    /// The mode of sessions the Claude Code plugin starts (no `--tend-mode` there).
    pub plugin_mode: Mode,
    /// A desktop notification when tend steers a loop or blocks an edit (unattended runs).
    pub notify: bool,
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
            experimental_audit: false,
            plugin_mode: Mode::Look,
            notify: false,
        }
    }
}

impl Config {
    /// Every knob as `name → value`, in a stable order.
    pub fn fields(&self) -> BTreeMap<String, String> {
        [
            ("idle_gap_minutes", self.idle_gap_minutes.to_string()),
            ("experimental_audit", self.experimental_audit.to_string()),
            ("keep_days", self.keep_days.to_string()),
            ("notify", self.notify.to_string()),
            (
                "plugin_mode",
                format!("{:?}", self.plugin_mode).to_lowercase(),
            ),
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
    /// The repo's `.tend.toml` (E7).
    policy: crate::policy::Policy,
    /// (actual, expected) values the latest failing run of each command showed (E4).
    failure_values: BTreeMap<String, Vec<(String, String)>>,
    /// Claude Code ran a call as written although tend rewrote it: wrap nothing more.
    rewrite_ignored: bool,
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

/// Prepare a test run's output for `observe`. Pure, so the daemon can run it without its
/// lock.
pub fn prepare(output: &str, level: u8, cfg: &Config) -> Prepared {
    prepare_for(output, None, level, cfg, true)
}

/// `prepare` with a hash the caller already has, and failures read only where loops
/// use them (test and build runs): a 16 MB file view is not parsed for nothing.
fn prepare_for(
    output: &str,
    hash: Option<String>,
    level: u8,
    cfg: &Config,
    failures: bool,
) -> Prepared {
    let tokens = estimate_tokens(output);
    let shaped = if tokens >= cfg.shape_threshold_tokens {
        shape(output, level, cfg.max_summary_ratio)
    } else {
        None
    };
    Prepared {
        hash: hash.unwrap_or_else(|| sha256_hex(output)),
        lines: output.lines().count(),
        tokens,
        failures: if failures {
            extract_failures(output)
        } else {
            Vec::new()
        },
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
    /// Id of the wrap action.
    action_id: String,
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
            policy: crate::policy::Policy::default(),
            failure_values: BTreeMap::new(),
            rewrite_ignored: false,
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

    /// What this session learned that the next one in the same project can start from:
    /// the shape level of each kind (E9).
    pub fn learned_levels(&self) -> BTreeMap<String, u8> {
        self.session
            .levels()
            .iter()
            .map(|(k, v)| (k.label().to_string(), *v))
            .collect()
    }

    /// Start from what earlier sessions in this project learned. Only kinds shaped by
    /// default can be carried over (C1: code reads and diffs are never shaped).
    pub fn restore_levels(&mut self, levels: &BTreeMap<String, u8>, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        for (label, level) in levels {
            let Some(kind) = OutputKind::from_label(label).filter(|k| k.shaped_by_default()) else {
                continue;
            };
            let from = self.session.shape_level(kind);
            let to = (*level).min(crate::session::MAX_LEVEL);
            if to == from {
                continue;
            }
            self.session.set_level(kind, to);
            self.push(
                &mut d,
                now_ms,
                vec![],
                Body::Adaptation {
                    knob: format!("shape_level.{label}"),
                    from: from.to_string(),
                    to: to.to_string(),
                    reason: "Carried over from earlier sessions in this project.".into(),
                },
            );
        }
        d
    }

    /// The repo's `.tend.toml`: extra test paths and protected paths for the guard.
    pub fn set_policy(&mut self, policy: crate::policy::Policy) {
        self.policy = policy;
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
            Event::ConversationChanged { .. } => {
                self.push(&mut d, now_ms, vec![], Body::Event { event });
                // Failures, edits, and blocks of the old conversation are not this one's.
                self.loops = LoopDetector::new(self.cfg.loop_threshold);
                self.last_summary = None;
                self.wrapped.clear();
                self.blocked.clear();
                self.last_blocked = None;
                self.allowed.clear();
                self.failure_values.clear();
            }
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
        // tend relies on `updatedInput` without a permission decision, which Claude Code
        // does not document (R3.1). If the call ran the agent's command, not the wrapper,
        // the wrap did not happen: say so, and stop wrapping (I6, F8).
        let ran_wrapper = input
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|c| c.contains(" run --id "));
        let ignored = wrapped.as_ref().filter(|_| !filtered && !ran_wrapper);
        let stored = Event::ToolCompleted {
            tool_use_id,
            tool,
            input,
            output: format!("<blob:{hash}>"),
            failed,
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        if let Some(w) = ignored {
            self.rewrite_ignored = true;
            self.push(
                d,
                now,
                vec![w.action_seq, ev],
                Body::Undelivered {
                    actions: vec![w.action_id.clone()],
                    reason: "Claude Code ran the command as written: it ignored tend's rewrite."
                        .into(),
                },
            );
        }
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
        let runs = matches!(info.kind, OutputKind::Test | OutputKind::Build);
        let level = self.session.shape_level(info.kind);
        let p = prepare_for(&output, Some(hash), level, &self.cfg, runs);
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
        // Values the failures show, by run, for the expected-to-actual rule (E4).
        if matches!(kind, OutputKind::Test | OutputKind::Build) {
            let pairs: Vec<(String, String)> = failures
                .iter()
                .flat_map(|f| guard::value_pairs(&f.message))
                .collect();
            if pairs.is_empty() {
                self.failure_values.remove(command);
            } else if self.failure_values.len() < 64 || self.failure_values.contains_key(command) {
                self.failure_values.insert(command.to_string(), pairs);
            }
        }
        let mut looped: Vec<(String, String, u64, u64)> = Vec::new();
        let active = self.active_ms;
        // The rules answer "same failure?" from the signature alone, so a judgment is
        // worth recording only when a decider can answer it differently (Phase 4).
        let judge = self.broker.has_decider_for("same_failure");
        for obs in self.loops.on_run(command, &failures, active) {
            if let Some(prev) = obs.previous_message.as_deref().filter(|_| judge) {
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
        // Rewriting every expected snapshot makes a failing suite pass by definition: say
        // so, so the agent (and the ledger) checks what changed (E5). It never blocks.
        if let Some(form) = crate::perception::snapshot_update(&command) {
            let message = format!(
                "tend: `{form}` rewrites expected snapshots or golden files. Check that only the intended ones change."
            );
            let action = ActionKind::Guard {
                path: "snapshots".into(),
                decision: GuardDecision::Warn,
            };
            if self.act(d, now, ev, action, message.clone()) {
                d.response.add_context.push(message);
            }
        }
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
        let reason = if self.rewrite_ignored {
            Some("Not wrapped: Claude Code ignored tend's command rewrite earlier in this session (check it with scripts/conformance.py).")
        } else if self.no_wrap.contains(&command_key(&command)) {
            Some("Not wrapped: a wrapped run of this command got no result before (denied or stopped).")
        } else {
            None
        };
        if let Some(reason) = reason {
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
        let (seq, action_id) = (self.next_seq, format!("a-{}", self.next_action));
        if self.act_if(d, now, cause, action, reason, supported) {
            d.response.wrap = true;
            self.wrapped.insert(
                tool_use_id,
                Wrapped {
                    command,
                    action_id,
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
        // Keep the texts only where the guard reads them, and only test-sized ones: an
        // edit of a lockfile or a data file is not worth a copy on disk (I4, F4).
        if guard::judges(&path, &old_text, &new_text) {
            for (hash, content) in [(old_hash, &old_text), (new_hash, &new_text)] {
                if content.len() <= MAX_EDIT_BLOB_BYTES {
                    d.blobs.push(Blob {
                        hash,
                        content: content.clone(),
                    });
                }
            }
        }
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
        let is_test = guard::judges(path, old, new) || self.policy.is_test(path);
        let protected = self.policy.protected_by(path).map(str::to_string);
        if !is_test && protected.is_none() {
            return None;
        }
        let q = Question::TestWeakened { path, old, new };
        let (answer, source) = if is_test {
            self.broker.ask(&q)
        } else {
            (Answer::Neutral, crate::broker::Source::Rule)
        };
        // E4: the edit makes the test expect what the failing run produced.
        let pairs: Vec<(String, String)> =
            self.failure_values.values().flatten().cloned().collect();
        let answer = match (answer, guard::expected_to_actual(old, new, &pairs)) {
            (Answer::Weakened { mut reasons }, Some(r)) => {
                reasons.push(r);
                Answer::Weakened { reasons }
            }
            (_, Some(r)) => Answer::Weakened { reasons: vec![r] },
            (a, None) => a,
        };
        // E7: the repo asks for a look at every edit of a protected path.
        let answer = match (answer, protected) {
            (a, None) => a,
            (Answer::Weakened { mut reasons }, Some(pat)) => {
                reasons.push(format!("the path is protected by .tend.toml (`{pat}`)"));
                Answer::Weakened { reasons }
            }
            (_, Some(pat)) => Answer::Weakened {
                reasons: vec![format!("the path is protected by .tend.toml (`{pat}`)")],
            },
        };
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
mod tests;
