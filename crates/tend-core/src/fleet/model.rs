//! The fleet state machine (F3b). Events in; records out. A record is the event itself
//! or a command with its reason, caused by that event. The hub (tend-cli) runs the
//! commands and feeds back what happened as events, so replaying the recorded events
//! gives the same records.
//!
//! Slots: a slot is one place a role's session runs. A slot id is `<role>-<n>` at the
//! top, and `<leader slot>/<role>-<n>` inside the team that slot leads.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::spec::{MsgKind, Role, Run, Spec, Verb, Workspace};

pub type SlotId = String;

/// Restarts of a crashed session before tend gives up on the slot.
pub const MAX_RESTARTS: u32 = 3;
/// First restart delay; it doubles each time.
const BACKOFF_MS: u64 = 5_000;
/// The delay after `n` earlier failures: doubles, up to 1024 times the first.
fn backoff(n: u32) -> u64 {
    BACKOFF_MS << n.min(10)
}
/// Messages per minute on one edge (sender → receiver), at most.
pub const RATE_PER_MIN: usize = 10;
/// A oneshot that asked a question waits this long for the answer (G2).
pub const QUESTION_WAIT_MS: u64 = 10 * 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Actor {
    User,
    Slot {
        id: SlotId,
    },
    /// tend itself (refusals, notices).
    Tend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Address {
    User,
    Slot {
        id: SlotId,
    },
    /// Every running session of a role, seen from the sender: its own level, or the
    /// team it leads; `parent` is the slot that leads the sender's team.
    Role {
        name: String,
    },
    /// Every other running session that shares the team worktree `name` (G3).
    Team {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FleetEvent {
    Up,
    Down,
    /// Time passed (the hub sends one every few seconds).
    Tick,
    /// The hub started the session for a slot.
    Started {
        slot: SlotId,
        session: String,
    },
    /// The slot's session ended. None: killed by a signal.
    Exited {
        slot: SlotId,
        code: Option<i32>,
    },
    /// A hook from the slot's session. `turn_ended`: it was the Stop hook.
    Activity {
        slot: SlotId,
        turn_ended: bool,
    },
    /// Run `n` sessions of a stream role: `worker`, or `api-1/worker` in a team.
    Scale {
        role: String,
        n: u32,
    },
    SpawnRequest {
        by: Actor,
        role: String,
    },
    StopRequest {
        by: Actor,
        slot: SlotId,
    },
    Message {
        from: Actor,
        to: Address,
        kind: MsgKind,
        text: String,
    },
    /// Put a task on the board (F5a). `role`: who should do it, at the adder's board.
    TaskAdd {
        by: Actor,
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        depends_on: Vec<String>,
        #[serde(default)]
        scope: Vec<String>,
        #[serde(default)]
        role: Option<String>,
        /// The command that decides whether it is done (F5b); else the fleet's.
        #[serde(default)]
        check: Option<String>,
    },
    /// Give a task to a role or a slot.
    TaskAssign {
        by: Actor,
        task: String,
        to: String,
    },
    /// Take a ready task (a given one, or the next one for the slot's role).
    TaskClaim {
        by: Actor,
        task: Option<String>,
    },
    TaskDone {
        by: Actor,
        task: String,
        summary: String,
    },
    TaskBlocked {
        by: Actor,
        task: String,
        reason: String,
    },
    /// The hub ran a task's check (F5b). `excerpt`: the failures, or the output's end.
    /// `flags`: what the audit of the task's diff found (experimental, opt-in).
    CheckResult {
        task: String,
        passed: bool,
        exit: Option<i32>,
        #[serde(default)]
        excerpt: String,
        #[serde(default)]
        flags: Vec<String>,
        /// Lines the task's worktree changed from the fleet's base (`best-of` rule, G9).
        #[serde(default)]
        diff_lines: Option<u64>,
    },
    /// The creator (or the user) sends a task under review back to work.
    TaskReject {
        by: Actor,
        task: String,
        reason: String,
    },
    /// A slot's turn used `tokens` (all prompt and output tokens); its session has cost
    /// `cost_usd` so far (F5d).
    Usage {
        slot: SlotId,
        tokens: u64,
        #[serde(default)]
        cost_usd: f64,
    },
    /// A session hit the plan's usage limit: every session would, so the fleet pauses.
    UsageLimit {
        slot: SlotId,
        text: String,
    },
    /// Stop handing out work and starting sessions; running turns finish.
    Pause,
    Resume,
    /// The hub started again after it stopped (G1): its sessions are gone.
    HubRestarted,
    /// A session in the user's terminal joins as an attached role (F4). `name` becomes
    /// its slot id, else `<role>-<n>`.
    Attached {
        role: String,
        #[serde(default)]
        name: Option<String>,
        session: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start a session for a slot. `leader` leads the team the slot is in.
    Spawn {
        slot: SlotId,
        role: String,
        leader: Option<SlotId>,
        agent: String,
        model: Option<String>,
        run: Run,
        workspace: Workspace,
        permission_mode: Option<String>,
        brief: String,
    },
    /// The user starts an attached role in a terminal; tend prints how.
    Attach {
        role: String,
    },
    Interrupt {
        slot: SlotId,
    },
    Stop {
        slot: SlotId,
    },
    /// Start the slot again at `at_ms` (after a crash). The hub only waits.
    Backoff {
        slot: SlotId,
        at_ms: u64,
    },
    Deliver {
        to: Address,
        from: Actor,
        kind: MsgKind,
        text: String,
    },
    /// Tell the user (inbox, notification).
    Notify {
        text: String,
    },
    /// The reply to the request this event was: refusals, and what a task verb did.
    Answer {
        to: Actor,
        text: String,
    },
    /// Run a task's check in its slot's worktree, and send back a CheckResult.
    RunCheck {
        task: String,
        slot: SlotId,
        command: String,
    },
    /// End a oneshot session: close its stdin. Its task is over, or never came back.
    Release {
        slot: SlotId,
    },
    /// Give the task to a fresh session at `at_ms`. The hub only waits.
    TaskRetry {
        task: String,
        at_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetRecord {
    pub seq: u64,
    pub ts_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<u64>,
    pub body: FleetBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetBody {
    Event {
        event: FleetEvent,
    },
    Command {
        id: String,
        command: Command,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    /// Waiting for room (limits), its leader, or a backoff.
    Waiting,
    Starting,
    Running,
    /// A oneshot session that finished.
    Done,
    Stopped,
    /// Crashed more than `MAX_RESTARTS` times.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub id: SlotId,
    pub role: String,
    /// The slot that leads this slot's team; None at the top.
    pub leader: Option<SlotId>,
    pub n: u32,
    pub state: SlotState,
    pub session: Option<String>,
    pub restarts: u32,
    pub retry_at: Option<u64>,
    /// A turn is in progress.
    pub busy: bool,
    pub last_seen_ms: u64,
    /// Stuck steps taken: 1 told the user, 2 interrupted, 3 stopped to restart.
    pub stuck: u8,
    /// Stopped on purpose: do not restart on exit.
    pub stopping: bool,
    /// The task this slot is bound to or working on.
    #[serde(default)]
    pub task: Option<String>,
    /// It was given a task once. A oneshot gets no second one: it ends after its turn.
    #[serde(default)]
    pub had_task: bool,
    /// Tokens its sessions used, and the current session's cost so far (F5d).
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    /// When the current session started.
    #[serde(default)]
    pub started_ms: u64,
    /// A oneshot asked a question at this time, and waits for the answer (G2).
    #[serde(default)]
    pub awaiting: Option<u64>,
}

impl Slot {
    fn live(&self) -> bool {
        matches!(self.state, SlotState::Starting | SlotState::Running)
    }

    fn active(&self) -> bool {
        matches!(
            self.state,
            SlotState::Waiting | SlotState::Starting | SlotState::Running
        )
    }
}

pub struct Model {
    spec: Spec,
    up: bool,
    slots: BTreeMap<SlotId, Slot>,
    counters: BTreeMap<String, u32>,
    next_seq: u64,
    next_action: u64,
    /// Message times per (sender, receiver), for the rate limit.
    edges: BTreeMap<(String, String), Vec<u64>>,
    /// The last kind sent on each edge, so a reply to a question is an answer (F4).
    last_kind: BTreeMap<(String, String), MsgKind>,
    /// The user was told the session limit was reached, and nothing freed up since.
    told_limit: bool,
    /// No new sessions or tasks (F5d): the user paused, a limit was hit.
    paused: bool,
    /// Cost of sessions that ended, so the fleet's total survives them.
    spent_usd: f64,
    /// The task board (F5a), by id, and the ids in the order tasks were added.
    tasks: BTreeMap<String, tasks::Task>,
    task_order: Vec<String>,
    /// Numbers given so far at the top (`t-N`) and under each parent task (`t-N.k`).
    task_numbers: BTreeMap<String, u32>,
}

impl Model {
    pub fn new(spec: Spec) -> Self {
        Self {
            spec,
            up: false,
            slots: BTreeMap::new(),
            counters: BTreeMap::new(),
            next_seq: 0,
            next_action: 1,
            edges: BTreeMap::new(),
            last_kind: BTreeMap::new(),
            told_limit: false,
            paused: false,
            spent_usd: 0.0,
            tasks: BTreeMap::new(),
            task_order: Vec::new(),
            task_numbers: BTreeMap::new(),
        }
    }

    pub fn slots(&self) -> &BTreeMap<SlotId, Slot> {
        &self.slots
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The slot is a session in the user's terminal (F4): tend never interrupts or
    /// stops it on its own.
    fn is_attached(&self, s: &Slot) -> bool {
        self.role_def(s).is_some_and(|r| r.run == Run::Attached)
    }

    /// The role a slot runs (its write scope, for the hub).
    pub fn role_of(&self, slot: &str) -> Option<&Role> {
        self.role_def(self.slots.get(slot)?)
    }

    /// Handle one event. The first record is the event; the rest are commands it caused.
    pub fn step(&mut self, event: FleetEvent, now: u64) -> Vec<FleetRecord> {
        let mut out = Vec::new();
        let ev = self.push(
            &mut out,
            now,
            vec![],
            FleetBody::Event {
                event: event.clone(),
            },
        );
        let mut cx = Cx {
            out: &mut out,
            now,
            cause: ev,
        };
        match event {
            FleetEvent::Up => self.on_up(&mut cx),
            FleetEvent::Down => self.on_down(&mut cx),
            FleetEvent::Tick => self.on_tick(&mut cx),
            FleetEvent::Started { slot, session } => self.on_started(&mut cx, &slot, session),
            FleetEvent::Exited { slot, code } => self.on_exited(&mut cx, &slot, code),
            FleetEvent::Activity { slot, turn_ended } => {
                if let Some(s) = self.slots.get_mut(&slot) {
                    s.last_seen_ms = now;
                    s.stuck = 0;
                    s.busy = !turn_ended;
                }
                if turn_ended {
                    self.after_turn(&mut cx, &slot);
                }
            }
            FleetEvent::Scale { role, n } => self.on_scale(&mut cx, &role, n),
            FleetEvent::SpawnRequest { by, role } => self.on_spawn_request(&mut cx, by, &role),
            FleetEvent::StopRequest { by, slot } => self.on_stop_request(&mut cx, by, &slot),
            FleetEvent::Message {
                from,
                to,
                kind,
                text,
            } => self.on_message(&mut cx, from, to, kind, text),
            FleetEvent::TaskAdd {
                by,
                title,
                body,
                depends_on,
                scope,
                role,
                check,
            } => self.on_task_add(
                &mut cx,
                by,
                tasks::NewTask {
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                    check,
                },
            ),
            FleetEvent::TaskAssign { by, task, to } => self.on_task_assign(&mut cx, by, &task, &to),
            FleetEvent::TaskClaim { by, task } => self.on_task_claim(&mut cx, by, task.as_deref()),
            FleetEvent::TaskDone { by, task, summary } => {
                self.on_task_end(&mut cx, by, &task, Ok(summary))
            }
            FleetEvent::TaskBlocked { by, task, reason } => {
                self.on_task_end(&mut cx, by, &task, Err(reason))
            }
            FleetEvent::CheckResult {
                task,
                passed,
                exit,
                excerpt,
                flags,
                diff_lines,
            } => {
                if let Some(t) = self.tasks.get_mut(&task) {
                    t.diff_lines = diff_lines.or(t.diff_lines);
                }
                self.on_check_result(&mut cx, &task, passed, exit, &excerpt, flags)
            }
            FleetEvent::TaskReject { by, task, reason } => {
                self.on_task_reject(&mut cx, by, &task, &reason)
            }
            FleetEvent::Usage {
                slot,
                tokens,
                cost_usd,
            } => self.on_usage(&mut cx, &slot, tokens, cost_usd),
            FleetEvent::UsageLimit { slot, text } => self.pause(
                &mut cx,
                &format!(
                    "{slot} hit the plan's usage limit ({}). The fleet pauses; run `tend fleet resume` when the limit resets.",
                    crate::text::truncate_chars(text.trim(), 120)
                ),
            ),
            FleetEvent::Pause => self.pause(&mut cx, "The user paused the fleet."),
            FleetEvent::HubRestarted => self.on_hub_restarted(&mut cx),
            FleetEvent::Attached {
                role,
                name,
                session,
            } => self.on_attached(&mut cx, &role, name.as_deref(), session),
            FleetEvent::Resume => {
                if self.paused {
                    self.paused = false;
                    self.told_limit = false;
                    self.cmd(
                        &mut cx,
                        Command::Notify {
                            text: "tend fleet: resumed.".into(),
                        },
                        "The user resumed the fleet.",
                    );
                    self.retry_waiting(&mut cx);
                }
            }
        }
        // Whatever changed, ready tasks may now have somewhere to go.
        self.dispatch(&mut cx);
        out
    }

    // --- Specs and scopes --------------------------------------------------------

    /// The spec of the team `leader` leads (the top for None).
    fn scope(&self, leader: Option<&str>) -> Option<&Spec> {
        match leader {
            None => Some(&self.spec),
            Some(l) => {
                let s = self.slots.get(l)?;
                self.role_def(s)?.team.as_deref()
            }
        }
    }

    fn role_def(&self, slot: &Slot) -> Option<&Role> {
        self.scope(slot.leader.as_deref())?.roles.get(&slot.role)
    }

    /// Slots in the team `leader` leads and in the teams below it.
    fn in_subtree(&self, slot: &Slot, leader: &str) -> bool {
        let mut at = slot.leader.clone();
        while let Some(l) = at {
            if l == leader {
                return true;
            }
            at = self.slots.get(&l).and_then(|s| s.leader.clone());
        }
        false
    }

    // --- Lifecycle ---------------------------------------------------------------

    fn on_up(&mut self, cx: &mut Cx) {
        if self.up {
            return self.cmd(
                cx,
                Command::Notify {
                    text: "The fleet is already up.".into(),
                },
                "Up while up.",
            );
        }
        self.up = true;
        self.start_team(cx, None);
    }

    /// The stream roles' minimum sessions, and attached roles for the user to start.
    /// Oneshot roles start per task (F5a).
    fn start_team(&mut self, cx: &mut Cx, leader: Option<&str>) {
        let Some(spec) = self.scope(leader) else {
            return;
        };
        let roles: Vec<(String, Run, u32)> = spec
            .roles
            .iter()
            .map(|(n, r)| (n.clone(), r.run, r.min))
            .collect();
        for (name, run, min) in roles {
            match run {
                Run::Stream => {
                    // A leader that starts again keeps its team: only the missing ones.
                    let have = self
                        .slots
                        .values()
                        .filter(|s| s.role == name && s.leader.as_deref() == leader && s.active())
                        .count() as u32;
                    for _ in have..min {
                        let id = self.new_slot(leader, &name);
                        self.try_spawn(cx, &id, "Its role starts with the fleet.");
                    }
                }
                Run::Attached if leader.is_none() => self.cmd(
                    cx,
                    Command::Attach { role: name.clone() },
                    &format!("Role {name} runs in the user's terminal; tend cannot start it."),
                ),
                _ => {}
            }
        }
    }

    fn new_slot(&mut self, leader: Option<&str>, role: &str) -> SlotId {
        let prefix = leader.map(|l| format!("{l}/")).unwrap_or_default();
        let key = format!("{prefix}{role}");
        // A terminal session may have been named `<key>-<n>`: skip taken ids.
        let mut n = self.counters.get(&key).copied().unwrap_or(0);
        let id = loop {
            n += 1;
            let id = format!("{key}-{n}");
            if !self.slots.contains_key(&id) {
                break id;
            }
        };
        self.counters.insert(key, n);
        self.slots.insert(
            id.clone(),
            Slot {
                id: id.clone(),
                role: role.into(),
                leader: leader.map(String::from),
                n,
                state: SlotState::Waiting,
                session: None,
                restarts: 0,
                retry_at: None,
                busy: false,
                last_seen_ms: 0,
                stuck: 0,
                stopping: false,
                task: None,
                had_task: false,
                tokens: 0,
                cost_usd: 0.0,
                started_ms: 0,
                awaiting: None,
            },
        );
        id
    }

    /// Why `id` cannot start now, if it cannot: the fleet is down, its leader is not
    /// running, or a session limit (the fleet's, or a team's above it) is full.
    fn blocked(&self, id: &str) -> Option<String> {
        let slot = self.slots.get(id)?;
        if !self.up {
            return Some("The fleet is down.".into());
        }
        if self.paused {
            return Some("The fleet is paused.".into());
        }
        if let Some(l) = &slot.leader {
            if self.slots.get(l).map(|s| s.state) != Some(SlotState::Running) {
                return Some(format!("Its leader {l} is not running."));
            }
        }
        let live = self.slots.values().filter(|s| s.live()).count() as u32;
        if live >= self.spec.limits.sessions {
            return Some(format!(
                "The fleet has its limit of {} sessions running.",
                self.spec.limits.sessions
            ));
        }
        // Each team above the slot caps the sessions below it.
        let mut at = slot.leader.clone();
        while let Some(l) = at {
            let leader = self.slots.get(&l)?;
            if let Some(team) = self.role_def(leader).and_then(|r| r.team.as_deref()) {
                let below = self
                    .slots
                    .values()
                    .filter(|s| s.live() && self.in_subtree(s, &l))
                    .count() as u32;
                if below >= team.limits.sessions {
                    return Some(format!(
                        "The team of {l} has its limit of {} sessions running.",
                        team.limits.sessions
                    ));
                }
            }
            at = leader.leader.clone();
        }
        None
    }

    fn try_spawn(&mut self, cx: &mut Cx, id: &str, why: &str) {
        if let Some(reason) = self.blocked(id) {
            if reason.contains("limit") && !self.told_limit {
                self.told_limit = true;
                self.cmd(
                    cx,
                    Command::Notify {
                        text: format!("tend fleet: {id} waits. {reason}"),
                    },
                    &reason,
                );
            }
            return;
        }
        let Some(slot) = self.slots.get(id) else {
            return;
        };
        let Some(role) = self.role_def(slot).cloned() else {
            return;
        };
        let (role_name, leader) = (slot.role.clone(), slot.leader.clone());
        if let Some(s) = self.slots.get_mut(id) {
            s.state = SlotState::Starting;
            s.retry_at = None;
            s.busy = false;
            s.stuck = 0;
        }
        self.cmd(
            cx,
            Command::Spawn {
                slot: id.into(),
                role: role_name,
                leader,
                agent: role.agent,
                model: role.model,
                run: role.run,
                workspace: role.workspace,
                permission_mode: role.permission_mode,
                brief: role.brief,
            },
            why,
        );
    }

    /// Start what waits and may start now: after a session ended, a leader started, or
    /// time passed. In slot order, so the result does not depend on timing.
    fn retry_waiting(&mut self, cx: &mut Cx) {
        let ready: Vec<SlotId> = self
            .slots
            .values()
            .filter(|s| s.state == SlotState::Waiting && s.retry_at.is_none_or(|t| t <= cx.now))
            .map(|s| s.id.clone())
            .collect();
        for id in ready {
            let why = if self.slots[&id].restarts > 0 {
                "Restart after its backoff."
            } else {
                "Room to start it."
            };
            self.try_spawn(cx, &id, why);
        }
    }

    fn on_started(&mut self, cx: &mut Cx, id: &str, session: String) {
        let Some(s) = self.slots.get_mut(id) else {
            return;
        };
        s.state = SlotState::Running;
        s.session = Some(session);
        s.started_ms = cx.now;
        s.last_seen_ms = cx.now;
        s.busy = false;
        let leads = self
            .slots
            .get(id)
            .and_then(|s| self.role_def(s))
            .is_some_and(|r| r.team.is_some());
        if leads {
            self.start_team(cx, Some(id));
        }
        self.retry_waiting(cx);
    }

    fn on_exited(&mut self, cx: &mut Cx, id: &str, code: Option<i32>) {
        let Some(slot) = self.slots.get(id).cloned() else {
            return;
        };
        let run = self.role_def(&slot).map(|r| r.run);
        self.told_limit = false;
        let restarts = slot.restarts + 1;
        // A oneshot is never restarted: its task, if any, goes to a fresh session. An
        // attached one was the user's terminal: it ended when the user left.
        let (state, retry_at) = if slot.stopping || !self.up || run == Some(Run::Attached) {
            (SlotState::Stopped, None)
        } else if run == Some(Run::Oneshot) {
            if code == Some(0) {
                (SlotState::Done, None)
            } else {
                (SlotState::Failed, None)
            }
        } else if restarts > MAX_RESTARTS {
            (SlotState::Failed, None)
        } else {
            let wait = backoff(restarts - 1);
            (SlotState::Waiting, Some(cx.now + wait))
        };
        let restarted =
            !matches!(run, Some(Run::Oneshot | Run::Attached)) && !slot.stopping && self.up;
        self.spent_usd += slot.cost_usd;
        if let Some(s) = self.slots.get_mut(id) {
            s.cost_usd = 0.0;
            s.busy = false;
            s.session = None;
            s.state = state;
            s.retry_at = retry_at;
            if restarted {
                s.restarts = restarts;
            }
        }
        let how = match code {
            Some(0) => "ended without `tend task done`".to_string(),
            Some(c) => format!("exited with code {c}"),
            None => "was killed".to_string(),
        };
        self.task_session_ended(cx, id, &how);
        if restarted {
            match (state, retry_at) {
                (SlotState::Failed, _) => self.cmd(
                    cx,
                    Command::Notify {
                        text: format!(
                            "tend fleet: {id} {how}, its {restarts}th crash. tend stopped restarting it."
                        ),
                    },
                    &format!("{id} crashed more than {MAX_RESTARTS} times."),
                ),
                (SlotState::Waiting, Some(at_ms)) => self.cmd(
                    cx,
                    Command::Backoff {
                        slot: id.into(),
                        at_ms,
                    },
                    &format!(
                        "{id} {how}; restart {restarts} of {MAX_RESTARTS} in {} s.",
                        (at_ms - cx.now) / 1000
                    ),
                ),
                _ => {}
            }
        }
        self.retry_waiting(cx);
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        self.retry_waiting(cx);
        // A session past its role's wall-time budget is stopped (F5d).
        let over: Vec<(SlotId, u64)> = self
            .slots
            .values()
            .filter(|s| s.state == SlotState::Running && !s.stopping)
            .filter(|s| !self.is_attached(s))
            .filter_map(|s| {
                let wall = self.role_def(s)?.budget.wall_ms?;
                (cx.now.saturating_sub(s.started_ms) > wall).then(|| (s.id.clone(), wall))
            })
            .collect();
        for (id, wall) in over {
            self.over_budget(
                cx,
                &id,
                &format!("ran past its wall-time budget of {} min", wall / 60_000),
            );
        }
        // A oneshot that waited too long for an answer ends (G2).
        let waited: Vec<SlotId> = self
            .slots
            .values()
            .filter(|s| s.live() && !s.busy)
            .filter(|s| {
                s.awaiting
                    .is_some_and(|t| cx.now.saturating_sub(t) > QUESTION_WAIT_MS)
            })
            .map(|s| s.id.clone())
            .collect();
        for id in waited {
            if let Some(s) = self.slots.get_mut(&id) {
                s.awaiting = None;
            }
            self.cmd(
                cx,
                Command::Release { slot: id.clone() },
                &format!(
                    "{id} asked a question and had no answer in {} min.",
                    QUESTION_WAIT_MS / 60_000
                ),
            );
        }
        let stuck_ms = |m: &Model, s: &Slot| {
            m.scope(s.leader.as_deref())
                .map_or(m.spec.stuck_ms, |sp| sp.stuck_ms)
        };
        let due: Vec<(SlotId, u8)> = self
            .slots
            .values()
            .filter(|s| {
                // A terminal session sends the hub no activity: it is the user's.
                s.state == SlotState::Running
                    && s.busy
                    && !self.is_attached(s)
                    && cx.now.saturating_sub(s.last_seen_ms)
                        >= stuck_ms(self, s).saturating_mul(u64::from(s.stuck) + 1)
            })
            .map(|s| (s.id.clone(), s.stuck + 1))
            .collect();
        for (id, step) in due {
            let mins = cx.now.saturating_sub(self.slots[&id].last_seen_ms) / 60_000;
            if let Some(s) = self.slots.get_mut(&id) {
                s.stuck = step;
            }
            match step {
                1 => self.cmd(
                    cx,
                    Command::Notify {
                        text: format!(
                            "tend fleet: {id} has shown no activity for {mins} min in a turn."
                        ),
                    },
                    &format!("{id} is busy with no hook for {mins} min."),
                ),
                2 => self.cmd(
                    cx,
                    Command::Interrupt { slot: id.clone() },
                    &format!("{id} is still silent after {mins} min; interrupt its turn."),
                ),
                _ => self.cmd(
                    cx,
                    Command::Stop { slot: id.clone() },
                    &format!("{id} did not answer an interrupt; stop it, to restart."),
                ),
            }
        }
    }

    fn on_down(&mut self, cx: &mut Cx) {
        self.up = false;
        let ids: Vec<SlotId> = self.slots.keys().cloned().collect();
        for id in ids {
            self.stop_slot(cx, &id, "The fleet is going down.");
        }
    }

    /// `role` is `worker` at the top, or `<leader slot>/worker` in a team.
    fn on_scale(&mut self, cx: &mut Cx, role: &str, n: u32) {
        let (leader, name) = match role.rsplit_once('/') {
            Some((l, r)) => (Some(l.to_string()), r.to_string()),
            None => (None, role.to_string()),
        };
        let Some(def) = self
            .scope(leader.as_deref())
            .and_then(|s| s.roles.get(&name))
        else {
            return self.refuse(cx, &Actor::User, &format!("There is no role {role}."));
        };
        if def.run != Run::Stream {
            return self.refuse(
                cx,
                &Actor::User,
                &format!("{role} is not a stream role; oneshot sessions start per task."),
            );
        }
        if n < def.min || n > def.max {
            return self.refuse(
                cx,
                &Actor::User,
                &format!(
                    "{role} runs {}..{} sessions; {n} is outside that.",
                    def.min, def.max
                ),
            );
        }
        let mut mine: Vec<Slot> = self
            .slots
            .values()
            .filter(|s| s.role == name && s.leader == leader && s.active())
            .cloned()
            .collect();
        let count = mine.len() as u32;
        if n > count {
            for _ in count..n {
                let id = self.new_slot(leader.as_deref(), &name);
                self.try_spawn(cx, &id, &format!("Scaled {role} to {n}."));
            }
        } else if n < count {
            // Stop the newest first, waiting ones before running ones, idle before busy.
            mine.sort_by_key(|s| (s.live(), s.busy, std::cmp::Reverse(s.n)));
            for s in mine.into_iter().take((count - n) as usize) {
                self.stop_slot(cx, &s.id, &format!("Scaled {role} to {n}."));
            }
        }
    }

    fn stop_slot(&mut self, cx: &mut Cx, id: &str, why: &str) {
        let Some(s) = self.slots.get_mut(id) else {
            return;
        };
        match s.state {
            SlotState::Waiting => s.state = SlotState::Stopped,
            SlotState::Starting | SlotState::Running => {
                s.stopping = true;
                self.cmd(cx, Command::Stop { slot: id.into() }, why);
            }
            _ => {}
        }
    }

    /// The level a slot's request acts on for `role`: the team it leads if the role is
    /// there, else its own level.
    fn level_for(&self, slot: &Slot, role: &str) -> Option<Option<String>> {
        let leads = self.role_def(slot).and_then(|r| r.team.as_deref());
        if leads.is_some_and(|t| t.roles.contains_key(role)) {
            return Some(Some(slot.id.clone()));
        }
        let own = self.scope(slot.leader.as_deref())?;
        own.roles.contains_key(role).then(|| slot.leader.clone())
    }

    fn on_spawn_request(&mut self, cx: &mut Cx, by: Actor, role: &str) {
        let leader = match &by {
            Actor::User | Actor::Tend => role.rsplit_once('/').map(|(l, _)| l.to_string()),
            Actor::Slot { id } => {
                let Some(s) = self.slots.get(id).cloned() else {
                    return;
                };
                if !self
                    .role_def(&s)
                    .is_some_and(|r| r.may.contains(&Verb::Spawn))
                {
                    return self.refuse(cx, &by, &format!("Role {} may not spawn.", s.role));
                }
                match self.level_for(&s, role) {
                    Some(l) => l,
                    None => {
                        return self.refuse(
                            cx,
                            &by,
                            &format!("There is no role {role} at {id}'s level or in its team."),
                        )
                    }
                }
            }
        };
        let name = role.rsplit_once('/').map_or(role, |(_, r)| r).to_string();
        let Some(def) = self
            .scope(leader.as_deref())
            .and_then(|s| s.roles.get(&name))
        else {
            return self.refuse(cx, &by, &format!("There is no role {role}."));
        };
        if def.run == Run::Attached {
            return self.refuse(cx, &by, &format!("{name} runs in the user's terminal."));
        }
        let max = def.max;
        let count = self
            .slots
            .values()
            .filter(|s| s.role == name && s.leader == leader && s.active())
            .count() as u32;
        if count >= max {
            return self.refuse(
                cx,
                &by,
                &format!("{name} already has its maximum of {max} sessions."),
            );
        }
        let id = self.new_slot(leader.as_deref(), &name);
        let why = match &by {
            Actor::Slot { id: s } => format!("{s} asked for another {name}."),
            _ => format!("The user asked for another {name}."),
        };
        self.try_spawn(cx, &id, &why);
    }

    fn on_stop_request(&mut self, cx: &mut Cx, by: Actor, id: &str) {
        let Some(target) = self.slots.get(id).cloned() else {
            return self.refuse(cx, &by, &format!("There is no slot {id}."));
        };
        if let Actor::Slot { id: s } = &by {
            let Some(me) = self.slots.get(s).cloned() else {
                return;
            };
            let may = self
                .role_def(&me)
                .is_some_and(|r| r.may.contains(&Verb::Spawn));
            let mine = target.leader.as_deref() == Some(s.as_str()) || self.in_subtree(&target, s);
            if !may || !mine {
                return self.refuse(
                    cx,
                    &by,
                    &format!("{s} may stop only sessions in the team it leads."),
                );
            }
        }
        let why = match &by {
            Actor::Slot { id: s } => format!("{s} asked to stop {id}."),
            _ => format!("The user asked to stop {id}."),
        };
        self.stop_slot(cx, id, &why);
    }

    // --- Messages ----------------------------------------------------------------

    /// Who a message reaches, or why it may not go. A sender sees its own level (its
    /// role name there) and, if it leads a team, that team (as `parent`).
    fn route(&self, from: &Actor, to: &Address, kind: MsgKind) -> Result<Vec<Address>, String> {
        let me = match from {
            Actor::Slot { id } => Some(self.slots.get(id).ok_or("unknown sender")?.clone()),
            _ => None,
        };
        // (spec, the sender's name in it, the level's leader) for each level it sees.
        let mut views: Vec<(&Spec, String, Option<String>)> = Vec::new();
        match &me {
            None => views.push((&self.spec, "user".into(), None)),
            Some(s) => {
                if let Some(team) = self.role_def(s).and_then(|r| r.team.as_deref()) {
                    views.push((team, "parent".into(), Some(s.id.clone())));
                }
                if let Some(own) = self.scope(s.leader.as_deref()) {
                    views.push((own, s.role.clone(), s.leader.clone()));
                }
            }
        }
        let me_id = me.as_ref().map(|s| s.id.clone());
        let deny = |to: &str| {
            let who = match &me {
                Some(s) => s.id.clone(),
                None => "The user".into(),
            };
            format!("{who} may not send {} to {to}.", a_kind(kind))
        };
        match to {
            Address::User => {
                if views
                    .iter()
                    .any(|(sp, name, _)| sp.allows(name, "user", kind))
                {
                    Ok(vec![Address::User])
                } else {
                    Err(deny("the user"))
                }
            }
            Address::Slot { id } => {
                let t = self
                    .slots
                    .get(id)
                    .ok_or(format!("There is no slot {id}."))?;
                // The receiver's name as the sender's level calls it.
                // The receiver's name in each level the sender sees: its role when it
                // is at that level, `parent` when it leads the sender's own level.
                let allowed = views.iter().any(|(sp, name, lvl)| {
                    let named = if t.leader == *lvl {
                        Some(t.role.as_str())
                    } else if lvl.as_ref() == Some(&t.id)
                        && *lvl == me.as_ref().and_then(|m| m.leader.clone())
                    {
                        Some("parent")
                    } else {
                        None
                    };
                    named.is_some_and(|n| sp.allows(name, n, kind))
                });
                if !allowed {
                    return Err(deny(id));
                }
                if !t.live() {
                    return Err(format!("{id} has no running session."));
                }
                Ok(vec![Address::Slot { id: id.clone() }])
            }
            Address::Team { name } => {
                if !matches!(kind, MsgKind::Notice | MsgKind::Question | MsgKind::Answer) {
                    return Err(format!(
                        "A team channel carries notices, questions, and answers, not {}.",
                        a_kind(kind)
                    ));
                }
                let ws = Workspace::Team { name: name.clone() };
                let in_team = |s: &Slot| self.role_def(s).is_some_and(|r| r.workspace == ws);
                if let Some(m) = &me {
                    if !in_team(m) {
                        return Err(format!("{} does not work in team {name}'s worktree.", m.id));
                    }
                }
                let found: Vec<Address> = self
                    .slots
                    .values()
                    .filter(|t| t.live() && in_team(t) && Some(&t.id) != me_id.as_ref())
                    .filter(|t| me.as_ref().is_none_or(|m| t.leader == m.leader))
                    .map(|t| Address::Slot { id: t.id.clone() })
                    .collect();
                if found.is_empty() {
                    return Err(format!("No other session of team {name} is running."));
                }
                Ok(found)
            }
            Address::Role { name } => {
                if name == "parent" {
                    let s = me.as_ref().ok_or("The user has no parent.")?;
                    let l = s.leader.clone().ok_or(format!("{} has no parent.", s.id))?;
                    return self.route(from, &Address::Slot { id: l }, kind);
                }
                let mut found = Vec::new();
                let mut any_level = false;
                for (sp, me_name, lvl) in &views {
                    if !sp.roles.contains_key(name) {
                        continue;
                    }
                    any_level = true;
                    if !sp.allows(me_name, name, kind) {
                        continue;
                    }
                    for t in self.slots.values() {
                        if t.role == *name
                            && t.leader == *lvl
                            && t.live()
                            && Some(&t.id) != me_id.as_ref()
                        {
                            found.push(Address::Slot { id: t.id.clone() });
                        }
                    }
                }
                if !any_level {
                    return Err(format!("There is no role {name} that this sender can see."));
                }
                if found.is_empty() {
                    return Err(if views.iter().any(|(sp, n, _)| sp.allows(n, name, kind)) {
                        format!("No session of {name} is running.")
                    } else {
                        deny(name)
                    });
                }
                Ok(found)
            }
        }
    }

    fn on_message(&mut self, cx: &mut Cx, from: Actor, to: Address, kind: MsgKind, text: String) {
        let targets = match self.route(&from, &to, kind) {
            Ok(t) => t,
            Err(reason) => return self.refuse(cx, &from, &reason),
        };
        let sender = match &from {
            Actor::Slot { id } => id.clone(),
            Actor::User => "user".into(),
            Actor::Tend => "tend".into(),
        };
        // A oneshot that asks while its task is open waits for the answer (G2).
        if let (Actor::Slot { id }, MsgKind::Question) = (&from, kind) {
            let oneshot = self
                .slots
                .get(id)
                .and_then(|s| self.role_def(s))
                .is_some_and(|r| r.run == Run::Oneshot);
            if let Some(s) = self
                .slots
                .get_mut(id)
                .filter(|s| oneshot && s.task.is_some())
            {
                s.awaiting = Some(cx.now);
            }
        }
        for t in targets {
            let receiver = match &t {
                Address::Slot { id } => id.clone(),
                _ => "user".into(),
            };
            let times = self
                .edges
                .entry((sender.clone(), receiver.clone()))
                .or_default();
            times.retain(|&at| cx.now.saturating_sub(at) < 60_000);
            let over = times.len() >= RATE_PER_MIN;
            if !over {
                times.push(cx.now);
            }
            if !over {
                self.last_kind
                    .insert((sender.clone(), receiver.clone()), kind);
            }
            if over {
                self.refuse(
                    cx,
                    &from,
                    &format!("More than {RATE_PER_MIN} messages a minute to {receiver}; this one was dropped."),
                );
                continue;
            }
            if let Address::Slot { id } = &t {
                if let Some(s) = self.slots.get_mut(id) {
                    // An answer (or anything) for a waiting asker is what it waited for.
                    s.awaiting = None;
                    if kind == MsgKind::Task {
                        s.busy = true;
                        s.last_seen_ms = cx.now;
                    }
                }
            }
            self.cmd(
                cx,
                Command::Deliver {
                    to: t,
                    from: from.clone(),
                    kind,
                    text: text.clone(),
                },
                &format!(
                    "The fleet's channels let {sender} send {} to {receiver}.",
                    a_kind(kind)
                ),
            );
        }
    }

    /// Tell the requester why not, as the reply to its request.
    fn refuse(&mut self, cx: &mut Cx, by: &Actor, reason: &str) {
        let command = Command::Answer {
            to: by.clone(),
            text: format!("tend: refused. {reason}"),
        };
        self.cmd(cx, command, reason);
    }

    // --- Hub restart (G1) -------------------------------------------------------------

    /// The hub came back. Its sessions ended with it (their stdin closed): stream slots
    /// start again, which is no crash; oneshot and terminal slots end. Their tasks go
    /// back to the queue without costing an attempt, and checks that ran run again.
    fn on_hub_restarted(&mut self, cx: &mut Cx) {
        if !self.up {
            return self.on_up(cx);
        }
        let live: Vec<SlotId> = self
            .slots
            .values()
            .filter(|s| s.live())
            .map(|s| s.id.clone())
            .collect();
        for id in &live {
            let run = self
                .slots
                .get(id)
                .and_then(|s| self.role_def(s))
                .map(|r| r.run);
            let cost = self.slots.get(id).map_or(0.0, |s| s.cost_usd);
            self.spent_usd += cost;
            if let Some(s) = self.slots.get_mut(id) {
                s.state = match run {
                    Some(Run::Stream) if !s.stopping => SlotState::Waiting,
                    _ => SlotState::Stopped,
                };
                s.session = None;
                s.busy = false;
                s.retry_at = None;
                s.stuck = 0;
                s.cost_usd = 0.0;
            }
        }
        let tasks: Vec<(String, TaskState, Option<SlotId>)> = self
            .tasks()
            .map(|t| (t.id.clone(), t.state, t.slot.clone()))
            .collect();
        for (tid, state, slot) in tasks {
            match state {
                TaskState::Working | TaskState::Bound => {
                    if let Some(sid) = &slot {
                        // A oneshot still waiting for room has nothing left to do.
                        let oneshot = self
                            .slots
                            .get(sid)
                            .and_then(|s| self.role_def(s))
                            .is_some_and(|r| r.run == Run::Oneshot);
                        if let Some(s) = self.slots.get_mut(sid) {
                            s.task = None;
                            if oneshot && s.state == SlotState::Waiting {
                                s.state = SlotState::Stopped;
                            }
                        }
                    }
                    let role = slot
                        .as_ref()
                        .and_then(|s| self.slots.get(s))
                        .map(|s| s.role.clone());
                    if let Some(t) = self.tasks.get_mut(&tid) {
                        t.state = TaskState::Queued;
                        t.slot = None;
                        if t.target.is_none() || t.target == slot {
                            t.target = role.or(t.target.clone());
                        }
                    }
                }
                TaskState::Checking => {
                    let t = self.tasks[&tid].clone();
                    if let (Some(command), Some(sid)) = (self.check_for(&t), slot) {
                        self.cmd(
                            cx,
                            Command::RunCheck {
                                task: tid.clone(),
                                slot: sid,
                                command,
                            },
                            &format!("The hub restarted while {tid}'s check ran; it runs again."),
                        );
                    }
                }
                _ => {}
            }
        }
        self.cmd(
            cx,
            Command::Notify {
                text: format!(
                    "tend fleet: the hub restarted; {} session{} ended with the old one. Stream sessions resume their conversations; their tasks go back to the queue.",
                    live.len(),
                    if live.len() == 1 { "" } else { "s" }
                ),
            },
            "The hub restarted.",
        );
        self.retry_waiting(cx);
    }

    // --- Attached sessions and message kinds (F4) -----------------------------------

    fn on_attached(&mut self, cx: &mut Cx, role: &str, name: Option<&str>, session: String) {
        let Some(def) = self.spec.roles.get(role) else {
            return self.refuse(
                cx,
                &Actor::User,
                &format!("There is no role {role} in this fleet."),
            );
        };
        if def.run != Run::Attached {
            return self.refuse(
                cx,
                &Actor::User,
                &format!("Role {role} is not attached (run = \"attached\"); tend starts its sessions itself."),
            );
        }
        let active = self
            .slots
            .values()
            .filter(|s| s.role == role && s.leader.is_none() && s.active())
            .count() as u32;
        if active >= def.max {
            return self.refuse(
                cx,
                &Actor::User,
                &format!(
                    "{role} already has its {} session{}.",
                    def.max,
                    if def.max == 1 { "" } else { "s" }
                ),
            );
        }
        let id = match name {
            Some(n) => {
                let ok = !n.is_empty()
                    && n.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                if !ok || self.slots.get(n).is_some_and(|s| s.active()) {
                    return self.refuse(
                        cx,
                        &Actor::User,
                        &format!(
                            "The name {n} is taken or not lowercase letters, digits, and `-`."
                        ),
                    );
                }
                self.slots.remove(n);
                let id = n.to_string();
                let slot = Slot {
                    id: id.clone(),
                    role: role.into(),
                    leader: None,
                    n: 1,
                    state: SlotState::Waiting,
                    session: None,
                    restarts: 0,
                    retry_at: None,
                    busy: false,
                    last_seen_ms: 0,
                    stuck: 0,
                    stopping: false,
                    task: None,
                    had_task: false,
                    tokens: 0,
                    cost_usd: 0.0,
                    started_ms: 0,
                    awaiting: None,
                };
                self.slots.insert(id.clone(), slot);
                id
            }
            None => self.new_slot(None, role),
        };
        if let Some(s) = self.slots.get_mut(&id) {
            s.state = SlotState::Running;
            s.session = Some(session);
            s.started_ms = cx.now;
            s.last_seen_ms = cx.now;
        }
        let said = id.clone();
        self.cmd(
            cx,
            Command::Answer {
                to: Actor::User,
                text: said,
            },
            &format!("A session in the user's terminal joined as {role}, named {id}."),
        );
    }

    /// The kind of message `from` sends `to` by default (`tend msg` without `--kind`):
    /// an answer when `to` last asked `from` a question, else the first the channels allow.
    pub fn default_kind(&self, from: &Actor, to: &Address) -> Option<MsgKind> {
        let me = match from {
            Actor::Slot { id } => id.clone(),
            _ => "user".to_string(),
        };
        let them = match to {
            Address::Slot { id } => Some(id.clone()),
            Address::User => Some("user".to_string()),
            Address::Role { .. } => None,
            Address::Team { .. } => return Some(MsgKind::Notice),
        };
        if let Some(them) = them {
            if self.last_kind.get(&(them, me)) == Some(&MsgKind::Question)
                && self.route(from, to, MsgKind::Answer).is_ok()
            {
                return Some(MsgKind::Answer);
            }
        }
        [
            MsgKind::Question,
            MsgKind::Answer,
            MsgKind::Report,
            MsgKind::Notice,
            MsgKind::Review,
            MsgKind::Handoff,
            MsgKind::Task,
        ]
        .into_iter()
        .find(|k| self.route(from, to, *k).is_ok())
    }

    // --- Budgets (F5d) ------------------------------------------------------------

    /// Tokens and cost the whole fleet used so far.
    pub fn spend(&self) -> (u64, f64) {
        let tokens = self
            .slots
            .values()
            .fold(0u64, |a, s| a.saturating_add(s.tokens));
        let live: f64 = self.slots.values().map(|s| s.cost_usd).sum();
        (tokens, self.spent_usd + live)
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    fn on_usage(&mut self, cx: &mut Cx, id: &str, tokens: u64, cost_usd: f64) {
        let Some(s) = self.slots.get_mut(id) else {
            return;
        };
        s.tokens = s.tokens.saturating_add(tokens);
        // A report that comes after the session ended is already in `spent_usd`.
        if s.live() {
            s.cost_usd = cost_usd;
        }
        let used = s.tokens;
        let budget = self
            .slots
            .get(id)
            .and_then(|s| self.role_def(s))
            .and_then(|r| r.budget.tokens);
        if let Some(b) = budget.filter(|b| used > *b) {
            self.over_budget(
                cx,
                id,
                &format!("used {used} tokens, over its budget of {b}"),
            );
        }
        if let Some(limit) = self.spec.limits.tokens {
            let (total, _) = self.spend();
            if total > limit && !self.paused {
                self.pause(
                    cx,
                    &format!("The fleet used {total} tokens, over its limit of {limit}. It pauses; raise limits.tokens or run `tend fleet resume`."),
                );
            }
        }
    }

    /// Stop a session that went over a budget; its task's attempt ends with it.
    fn over_budget(&mut self, cx: &mut Cx, id: &str, why: &str) {
        let Some(s) = self.slots.get(id) else {
            return;
        };
        if s.stopping || !s.live() {
            return;
        }
        if let Some(s) = self.slots.get_mut(id) {
            s.stopping = true;
        }
        self.cmd(
            cx,
            Command::Stop { slot: id.into() },
            &format!("{id} {why}."),
        );
        self.cmd(
            cx,
            Command::Notify {
                text: format!("tend fleet: stopped {id}: it {why}."),
            },
            &format!("{id} {why}."),
        );
    }

    fn pause(&mut self, cx: &mut Cx, why: &str) {
        if self.paused {
            return;
        }
        self.paused = true;
        self.cmd(
            cx,
            Command::Notify {
                text: format!("tend fleet: paused. {why}"),
            },
            why,
        );
    }

    // --- Records -----------------------------------------------------------------

    fn cmd(&mut self, cx: &mut Cx, command: Command, reason: &str) {
        let id = format!("a-{}", self.next_action);
        self.next_action += 1;
        let cause = cx.cause;
        self.push(
            cx.out,
            cx.now,
            vec![cause],
            FleetBody::Command {
                id,
                command,
                reason: reason.into(),
            },
        );
    }

    fn push(
        &mut self,
        out: &mut Vec<FleetRecord>,
        now: u64,
        causes: Vec<u64>,
        body: FleetBody,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        out.push(FleetRecord {
            seq,
            ts_ms: now,
            causes,
            body,
        });
        seq
    }
}

/// `a question`, `an answer`.
fn a_kind(k: MsgKind) -> String {
    let name = kind_name(k);
    let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {name}")
}

fn kind_name(k: MsgKind) -> &'static str {
    match k {
        MsgKind::Task => "task",
        MsgKind::Report => "report",
        MsgKind::Question => "question",
        MsgKind::Answer => "answer",
        MsgKind::Review => "review",
        MsgKind::Notice => "notice",
        MsgKind::Handoff => "handoff",
    }
}

/// What one step writes to.
struct Cx<'a> {
    out: &'a mut Vec<FleetRecord>,
    now: u64,
    cause: u64,
}

mod tasks;
pub use tasks::{Task, TaskState};

#[cfg(test)]
mod tests;
