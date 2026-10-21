//! The task board (F5a). A task lives on one board: the top (`t-N`), or the team of the
//! slot that leads it. A task added by a slot that is working on a task is part of that
//! task (`t-N.k`), and the parent is done only when its parts are.
//!
//! A queued task is for a role or a slot. `dispatch` gives a ready one (its dependencies
//! done) to an idle stream session of its role, or starts a fresh oneshot session for it.
//! A session that ends without `done` or `blocked` costs the task an attempt; past the
//! role's budget the task fails, and its creator is told.
//!
//! The check gate (F5b): a session's `done` on a task with a check (its own, or the
//! fleet's) runs the check first. Passed: done. Failed: an attempt, and the failure goes
//! back to the same session if it is open, else to a fresh one. Passed but flagged by
//! the audit (experimental, opt-in): its creator reviews it (`done` or `reject`).

use super::*;

/// A task as `tend task add` gives it.
pub struct NewTask {
    pub title: String,
    pub body: String,
    pub depends_on: Vec<String>,
    pub scope: Vec<String>,
    pub role: Option<String>,
    pub check: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// On the board, for no one yet (`tend task assign` gives it out).
    Open,
    /// For a role or a slot; waits for its dependencies or a session.
    Queued,
    /// A session was chosen and is starting.
    Bound,
    Working,
    /// The session said done; tend runs the check.
    Checking,
    /// The check passed, but the audit flagged the diff: the creator decides.
    Review,
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub body: String,
    pub depends_on: Vec<String>,
    pub scope: Vec<String>,
    /// The slot that leads the board's level; None for the top.
    pub board: Option<SlotId>,
    pub parent: Option<String>,
    pub created_by: Actor,
    /// A role name at the board's level, or a slot id.
    pub target: Option<String>,
    pub slot: Option<SlotId>,
    pub state: TaskState,
    pub attempts: u32,
    pub not_before: Option<u64>,
    /// What `done` or `blocked` said.
    pub outcome: Option<String>,
    /// The task's own check, if any (else the fleet's).
    #[serde(default)]
    pub check: Option<String>,
    /// Why the last attempt did not count: a failed check, or a reviewer's reason.
    #[serde(default)]
    pub last_check: Option<String>,
}

impl Model {
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.task_order.iter().filter_map(|id| self.tasks.get(id))
    }

    /// The board an actor works on, and its name there.
    fn board_of(&self, by: &Actor) -> Option<(Option<SlotId>, String)> {
        match by {
            Actor::User | Actor::Tend => Some((None, "user".into())),
            Actor::Slot { id } => {
                let s = self.slots.get(id)?;
                if self.role_def(s).is_some_and(|r| r.team.is_some()) {
                    Some((Some(id.clone()), "parent".into()))
                } else {
                    Some((s.leader.clone(), s.role.clone()))
                }
            }
        }
    }

    fn may(&self, by: &Actor, verb: Verb) -> bool {
        match by {
            Actor::Slot { id } => self
                .slots
                .get(id)
                .and_then(|s| self.role_def(s))
                .is_some_and(|r| r.may.contains(&verb)),
            _ => true,
        }
    }

    pub(super) fn on_task_add(&mut self, cx: &mut Cx, by: Actor, t: NewTask) {
        if !self.may(&by, Verb::TaskAdd) {
            return self.refuse(cx, &by, "This role may not add tasks.");
        }
        let Some((board, name)) = self.board_of(&by) else {
            return;
        };
        let Some(spec) = self.scope(board.as_deref()) else {
            return;
        };
        let target = match &t.role {
            Some(r) if !spec.roles.contains_key(r) => {
                return self.refuse(cx, &by, &format!("There is no role {r} on this board."));
            }
            Some(r) if !spec.allows(&name, r, MsgKind::Task) && name != "user" => {
                return self.refuse(cx, &by, &format!("{name} may not give tasks to {r}."));
            }
            Some(r) => Some(r.clone()),
            // The one role this actor may give tasks to, if there is one.
            None => {
                let mut to: Vec<&String> = spec
                    .roles
                    .keys()
                    .filter(|r| **r != name && spec.allows(&name, r, MsgKind::Task))
                    .collect();
                if to.len() == 1 {
                    to.pop().cloned()
                } else {
                    None
                }
            }
        };
        if let Some(d) = t.depends_on.iter().find(|d| !self.tasks.contains_key(*d)) {
            return self.refuse(cx, &by, &format!("There is no task {d} to depend on."));
        }
        let parent = match &by {
            Actor::Slot { id } => self.slots.get(id).and_then(|s| s.task.clone()),
            _ => None,
        };
        let key = parent.clone().unwrap_or_default();
        let n = self.task_numbers.entry(key).or_insert(0);
        *n += 1;
        let id = match &parent {
            Some(p) => format!("{p}.{n}"),
            None => format!("t-{n}"),
        };
        let state = if target.is_some() {
            TaskState::Queued
        } else {
            TaskState::Open
        };
        let said = match &target {
            Some(r) => format!("Added {id}, for {r}."),
            None => format!("Added {id}. Give it out with `tend task assign {id} <role>`."),
        };
        self.task_order.push(id.clone());
        self.tasks.insert(
            id.clone(),
            Task {
                id: id.clone(),
                title: t.title,
                body: t.body,
                depends_on: t.depends_on,
                scope: t.scope,
                board,
                parent,
                created_by: by.clone(),
                target,
                slot: None,
                state,
                attempts: 0,
                not_before: None,
                outcome: None,
                check: t.check,
                last_check: None,
            },
        );
        self.cmd(
            cx,
            Command::Answer {
                to: by,
                text: said.clone(),
            },
            &said,
        );
    }

    pub(super) fn on_task_assign(&mut self, cx: &mut Cx, by: Actor, task: &str, to: &str) {
        if !self.may(&by, Verb::TaskAssign) {
            return self.refuse(cx, &by, "This role may not assign tasks.");
        }
        let Some(t) = self.tasks.get(task).cloned() else {
            return self.refuse(cx, &by, &format!("There is no task {task}."));
        };
        let Some((board, name)) = self.board_of(&by) else {
            return;
        };
        if by != Actor::User && board != t.board {
            return self.refuse(cx, &by, &format!("{task} is on another board."));
        }
        if !matches!(t.state, TaskState::Open | TaskState::Queued) {
            return self.refuse(
                cx,
                &by,
                &format!(
                    "{task} is {:?}; only open or queued tasks are assigned.",
                    t.state
                )
                .replace("Working", "being worked on"),
            );
        }
        let role = match self.slots.get(to) {
            Some(s) if s.leader == t.board => s.role.clone(),
            Some(_) => return self.refuse(cx, &by, &format!("{to} is not on {task}'s board.")),
            None => to.to_string(),
        };
        let Some(spec) = self.scope(t.board.as_deref()) else {
            return;
        };
        if !spec.roles.contains_key(&role) {
            return self.refuse(
                cx,
                &by,
                &format!("There is no role or slot {to} on {task}'s board."),
            );
        }
        if name != "user" && !spec.allows(&name, &role, MsgKind::Task) {
            return self.refuse(cx, &by, &format!("{name} may not give tasks to {role}."));
        }
        if let Some(x) = self.tasks.get_mut(task) {
            x.target = Some(to.to_string());
            x.state = TaskState::Queued;
        }
        let said = format!("{task} is for {to}.");
        self.cmd(
            cx,
            Command::Answer {
                to: by,
                text: said.clone(),
            },
            &said,
        );
    }

    fn ready(&self, t: &Task, now: u64) -> bool {
        t.depends_on.iter().all(|d| {
            self.tasks
                .get(d)
                .is_some_and(|x| x.state == TaskState::Done)
        }) && t.not_before.is_none_or(|nb| nb <= now)
    }

    pub(super) fn on_task_claim(&mut self, cx: &mut Cx, by: Actor, task: Option<&str>) {
        let id = match &by {
            Actor::Slot { id } => id.clone(),
            _ => return self.refuse(cx, &by, "Only a fleet session claims tasks."),
        };
        let id = &id;
        let Some(s) = self.slots.get(id).cloned() else {
            return;
        };
        if let Some(cur) = &s.task {
            return self.refuse(cx, &by, &format!("{id} already has {cur}."));
        }
        let fits = |t: &Task| {
            t.board == s.leader
                && matches!(t.state, TaskState::Open | TaskState::Queued)
                && t.target.as_ref().is_none_or(|x| *x == s.role || x == id)
        };
        let pick = match task {
            Some(tid) => match self.tasks.get(tid) {
                Some(t) if fits(t) && self.ready(t, cx.now) => Some(tid.to_string()),
                Some(_) => {
                    return self.refuse(
                        cx,
                        &by,
                        &format!("{tid} is not a ready task for {}.", s.role),
                    )
                }
                None => return self.refuse(cx, &by, &format!("There is no task {tid}.")),
            },
            None => self
                .tasks()
                .find(|t| fits(t) && self.ready(t, cx.now))
                .map(|t| t.id.clone()),
        };
        let Some(tid) = pick else {
            return self.refuse(cx, &by, &format!("No ready task for {}.", s.role));
        };
        self.bind(&tid, id, cx.now);
        if let Some(t) = self.tasks.get_mut(&tid) {
            t.state = TaskState::Working;
        }
        let text = self.task_text(&tid);
        self.cmd(
            cx,
            Command::Answer { to: by, text },
            &format!("{id} claimed {tid}."),
        );
    }

    pub(super) fn on_task_end(
        &mut self,
        cx: &mut Cx,
        by: Actor,
        task: &str,
        outcome: Result<String, String>,
    ) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return self.refuse(cx, &by, &format!("There is no task {task}."));
        };
        // Who may: its own session, the user, and its creator once it is under review.
        // Under review, the task is its creator's, not the session's that did it.
        let own = t.state != TaskState::Review
            && matches!(&by, Actor::Slot { id } if t.slot.as_deref() == Some(id.as_str()));
        let user = by == Actor::User;
        let reviewer = t.state == TaskState::Review && by == t.created_by;
        if !(own || user || reviewer) {
            return self.refuse(cx, &by, &format!("{task} is not this session's task."));
        }
        if !matches!(
            t.state,
            TaskState::Working
                | TaskState::Bound
                | TaskState::Queued
                | TaskState::Open
                | TaskState::Review
        ) {
            return self.refuse(
                cx,
                &by,
                &format!("{task} is already {:?}.", t.state).to_lowercase(),
            );
        }
        if outcome.is_ok() {
            let open = self
                .tasks()
                .find(|c| c.parent.as_deref() == Some(task) && c.state != TaskState::Done)
                .map(|c| c.id.clone());
            if let Some(o) = open {
                return self.refuse(cx, &by, &format!("{o}, a part of {task}, is not done yet."));
            }
        }
        // The gate: the session's own `done` counts once its check passes.
        if let (Ok(summary), true, Some(slot)) = (&outcome, own, &t.slot) {
            if let Some(check) = self.check_for(&t) {
                if let Some(x) = self.tasks.get_mut(task) {
                    x.state = TaskState::Checking;
                    x.outcome = Some(summary.clone());
                }
                self.cmd(
                    cx,
                    Command::RunCheck {
                        task: task.into(),
                        slot: slot.clone(),
                        command: check.clone(),
                    },
                    &format!("{slot} says {task} is done; it counts once `{check}` passes."),
                );
                let said = format!(
                    "{task}: tend runs its check (`{check}`) now. If it fails, you get the failure to fix."
                );
                return self.cmd(
                    cx,
                    Command::Answer {
                        to: by,
                        text: said.clone(),
                    },
                    &said,
                );
            }
        }
        let who = if reviewer {
            match &by {
                Actor::Slot { id } => format!("accepted by {id}"),
                _ => "accepted by the user".into(),
            }
        } else {
            t.slot.clone().unwrap_or_else(|| "the user".into())
        };
        let (state, kind, text) = match &outcome {
            Ok(summary) => (
                TaskState::Done,
                MsgKind::Report,
                format!("{task} is done ({who}): {summary}"),
            ),
            Err(reason) => (
                TaskState::Blocked,
                MsgKind::Question,
                format!("{task} is blocked ({who}): {reason}"),
            ),
        };
        let note = match outcome {
            Ok(s) | Err(s) => s,
        };
        self.finish(cx, task, state, note);
        if !reviewer {
            self.tell_creator(cx, &t.created_by, kind, &text);
        }
        let said = match state {
            TaskState::Done => format!("{task} is done."),
            _ => format!("{task} is blocked; its creator is told."),
        };
        self.cmd(
            cx,
            Command::Answer {
                to: by,
                text: said.clone(),
            },
            &said,
        );
    }

    /// The check that decides a task: its own, its board's, or the fleet's.
    fn check_for(&self, t: &Task) -> Option<String> {
        t.check
            .clone()
            .or_else(|| self.scope(t.board.as_deref()).and_then(|s| s.check.clone()))
            .or_else(|| self.spec.check.clone())
    }

    fn budget_for(&self, t: &Task, role: &str) -> u32 {
        self.scope(t.board.as_deref())
            .and_then(|sp| sp.roles.get(role))
            .map_or(3, |r| r.budget.attempts.max(1))
    }

    /// End a task: free its session, which ends now if it is a oneshot whose turn is over.
    fn finish(&mut self, cx: &mut Cx, task: &str, state: TaskState, note: String) {
        let slot = self.tasks.get(task).and_then(|t| t.slot.clone());
        if let Some(x) = self.tasks.get_mut(task) {
            x.state = state;
            x.outcome = Some(note);
        }
        if let Some(sid) = slot {
            if let Some(s) = self.slots.get_mut(&sid) {
                if s.task.as_deref() == Some(task) {
                    s.task = None;
                }
            }
            self.release_if_idle(cx, &sid);
        }
    }

    /// A oneshot whose turn is over and whose task is over: end its session.
    fn release_if_idle(&mut self, cx: &mut Cx, slot: &str) {
        let Some(s) = self.slots.get(slot) else {
            return;
        };
        let oneshot = self.role_def(s).is_some_and(|r| r.run == Run::Oneshot);
        if oneshot && s.live() && !s.busy && s.task.is_none() && s.had_task {
            self.cmd(
                cx,
                Command::Release { slot: slot.into() },
                &format!("{slot} is a oneshot and its task is over."),
            );
        }
    }

    /// A slot ended a turn. A oneshot that had a task ends now, unless its task is being
    /// checked (a failure comes back to it). One that ends with its task still open
    /// costs that task an attempt when it exits.
    pub(super) fn after_turn(&mut self, cx: &mut Cx, slot: &str) {
        let Some(s) = self.slots.get(slot) else {
            return;
        };
        if !s.had_task || !self.role_def(s).is_some_and(|r| r.run == Run::Oneshot) {
            return;
        }
        let checking = s
            .task
            .as_ref()
            .and_then(|t| self.tasks.get(t))
            .is_some_and(|t| t.state == TaskState::Checking);
        if !checking {
            self.cmd(
                cx,
                Command::Release { slot: slot.into() },
                &format!("{slot} is a oneshot and ended its turn."),
            );
        }
    }

    pub(super) fn on_check_result(
        &mut self,
        cx: &mut Cx,
        task: &str,
        passed: bool,
        exit: Option<i32>,
        excerpt: &str,
        flags: Vec<String>,
    ) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return;
        };
        if t.state != TaskState::Checking {
            return;
        }
        let check = self.check_for(&t).unwrap_or_default();
        let who = t.slot.clone().unwrap_or_else(|| "its session".into());
        let summary = t.outcome.clone().unwrap_or_default();
        if passed && flags.is_empty() {
            self.finish(cx, task, TaskState::Done, summary.clone());
            let text = format!("{task} is done ({who}), and `{check}` passed: {summary}");
            return self.tell_creator(cx, &t.created_by, MsgKind::Report, &text);
        }
        if passed {
            self.finish(cx, task, TaskState::Review, summary);
            let text = format!(
                "{task} passed `{check}`, but tend's audit (experimental) flags its diff: {}. Accept it with `tend task done {task}`, or send it back with `tend task reject {task} \"<reason>\"`.",
                flags.join("; ")
            );
            return self.tell_creator(cx, &t.created_by, MsgKind::Question, &text);
        }
        let exit = exit.map_or("killed".to_string(), |c| format!("exit {c}"));
        let failure = format!(
            "The check `{check}` failed ({exit}):\n{}",
            crate::text::truncate_chars(excerpt.trim(), 2000)
        );
        self.retry(cx, task, &who, failure);
    }

    pub(super) fn on_task_reject(&mut self, cx: &mut Cx, by: Actor, task: &str, reason: &str) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return self.refuse(cx, &by, &format!("There is no task {task}."));
        };
        if t.state != TaskState::Review {
            return self.refuse(cx, &by, &format!("{task} is not waiting for review."));
        }
        if by != Actor::User && by != t.created_by {
            return self.refuse(
                cx,
                &by,
                &format!("Only {task}'s creator or the user reviews it."),
            );
        }
        let who = match &by {
            Actor::Slot { id } => id.clone(),
            _ => "the user".into(),
        };
        self.retry(
            cx,
            task,
            "its session",
            format!("Sent back by {who}: {reason}"),
        );
        let said = format!("{task} goes back to work.");
        self.cmd(
            cx,
            Command::Answer {
                to: by,
                text: said.clone(),
            },
            &said,
        );
    }

    /// One attempt did not count. The task goes back to its open session, which made the
    /// change, or to a fresh one after a backoff; past the budget it fails.
    fn retry(&mut self, cx: &mut Cx, task: &str, who: &str, why: String) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return;
        };
        let role = t
            .slot
            .as_ref()
            .and_then(|s| self.slots.get(s))
            .map(|s| s.role.clone())
            .or_else(|| t.target.clone())
            .unwrap_or_default();
        let budget = self.budget_for(&t, &role);
        let attempts = t.attempts + 1;
        if let Some(x) = self.tasks.get_mut(task) {
            x.attempts = attempts;
            x.last_check = Some(why.clone());
        }
        let first = why.lines().next().unwrap_or("").to_string();
        if attempts >= budget {
            self.finish(cx, task, TaskState::Failed, why);
            let text = format!("{task} failed after {attempts} attempts ({who}): {first}");
            return self.tell_creator(cx, &t.created_by, MsgKind::Report, &text);
        }
        let open = t.slot.clone().filter(|s| {
            self.slots
                .get(s)
                .is_some_and(|x| x.state == SlotState::Running && x.task.as_deref() == Some(task))
        });
        if let Some(slot) = open {
            if let Some(x) = self.tasks.get_mut(task) {
                x.state = TaskState::Bound;
            }
            if let Some(s) = self.slots.get_mut(&slot) {
                s.busy = true;
            }
            return self.deliver_task(cx, task, &slot);
        }
        let at_ms = cx.now + (BACKOFF_MS << (attempts - 1));
        if let Some(sid) = &t.slot {
            if let Some(s) = self.slots.get_mut(sid) {
                if s.task.as_deref() == Some(task) {
                    s.task = None;
                }
            }
        }
        if let Some(x) = self.tasks.get_mut(task) {
            x.state = TaskState::Queued;
            x.slot = None;
            x.not_before = Some(at_ms);
            if x.target.is_none() || x.target.as_deref() == t.slot.as_deref() {
                x.target = Some(role.clone());
            }
        }
        if let Some(sid) = &t.slot {
            self.release_if_idle(cx, sid);
        }
        self.cmd(
            cx,
            Command::TaskRetry {
                task: task.into(),
                at_ms,
            },
            &format!(
                "{task}: {first} Attempt {} of {budget} goes to a fresh session in {} s.",
                attempts + 1,
                (at_ms - cx.now) / 1000
            ),
        );
    }

    /// A report to whoever created a task: its session, or the user.
    fn tell_creator(&mut self, cx: &mut Cx, creator: &Actor, kind: MsgKind, text: &str) {
        match creator {
            Actor::Slot { id } if self.slots.get(id).is_some_and(|s| s.live()) => self.cmd(
                cx,
                Command::Deliver {
                    to: Address::Slot { id: id.clone() },
                    from: Actor::Tend,
                    kind,
                    text: text.into(),
                },
                &format!("{id} created the task, so it hears how it ended."),
            ),
            _ => self.cmd(
                cx,
                Command::Notify {
                    text: format!("tend fleet: {text}"),
                },
                "The user created the task, or its creator is gone.",
            ),
        }
    }

    /// A slot's session ended: its unfinished task costs an attempt, and goes back to
    /// the queue for a fresh session, or fails past the role's budget.
    pub(super) fn task_session_ended(&mut self, cx: &mut Cx, slot: &str, how: &str) {
        let Some(tid) = self.slots.get_mut(slot).and_then(|s| s.task.take()) else {
            return;
        };
        let Some(t) = self.tasks.get(&tid).cloned() else {
            return;
        };
        if t.state == TaskState::Checking {
            if let Some(x) = self.tasks.get_mut(&tid) {
                x.slot = None;
            }
            return;
        }
        if !matches!(t.state, TaskState::Working | TaskState::Bound) {
            return;
        }
        let role = self
            .slots
            .get(slot)
            .map(|s| s.role.clone())
            .unwrap_or_default();
        let budget = self
            .scope(t.board.as_deref())
            .and_then(|sp| sp.roles.get(&role))
            .map_or(3, |r| r.budget.attempts.max(1));
        let attempts = t.attempts + 1;
        if attempts >= budget {
            if let Some(x) = self.tasks.get_mut(&tid) {
                x.attempts = attempts;
                x.state = TaskState::Failed;
                x.slot = None;
                x.outcome = Some(format!("its session {how}"));
            }
            let text = format!("{tid} failed: its session {how}, attempt {attempts} of {budget}.");
            self.tell_creator(cx, &t.created_by, MsgKind::Report, &text);
            return;
        }
        let at_ms = cx.now + (BACKOFF_MS << (attempts - 1));
        if let Some(x) = self.tasks.get_mut(&tid) {
            x.attempts = attempts;
            x.state = TaskState::Queued;
            x.slot = None;
            x.not_before = Some(at_ms);
            // A task for a slot that is gone goes to the slot's role.
            if x.target.as_deref() == Some(slot) {
                x.target = Some(role.clone());
            }
        }
        self.cmd(
            cx,
            Command::TaskRetry {
                task: tid.clone(),
                at_ms,
            },
            &format!(
                "{tid}: its session {slot} {how}; attempt {} of {budget} in {} s.",
                attempts + 1,
                (at_ms - cx.now) / 1000
            ),
        );
    }

    fn bind(&mut self, tid: &str, slot: &str, now: u64) {
        if let Some(t) = self.tasks.get_mut(tid) {
            t.slot = Some(slot.to_string());
            t.state = TaskState::Bound;
        }
        if let Some(s) = self.slots.get_mut(slot) {
            s.task = Some(tid.to_string());
            s.had_task = true;
            s.busy = true;
            s.last_seen_ms = now;
        }
    }

    /// A running session with nothing to do, that may take a task: a oneshot only
    /// before its first (the hub closes its stdin after that turn).
    fn takes_work(&self, s: &Slot) -> bool {
        s.state == SlotState::Running
            && !s.busy
            && s.task.is_none()
            && !(s.had_task && self.role_def(s).is_some_and(|r| r.run == Run::Oneshot))
    }

    /// Give ready tasks to sessions: first to the slots they are bound to, once those
    /// run; then queued ones, to an idle stream session of their role or to a new
    /// oneshot session. In board order, so the result does not depend on timing.
    pub(super) fn dispatch(&mut self, cx: &mut Cx) {
        if !self.up || self.paused {
            return;
        }
        let bound: Vec<(SlotId, String)> = self
            .slots
            .values()
            .filter(|s| s.state == SlotState::Running)
            .filter_map(|s| {
                let t = s.task.as_ref()?;
                (self.tasks.get(t)?.state == TaskState::Bound).then(|| (s.id.clone(), t.clone()))
            })
            .collect();
        for (slot, tid) in bound {
            self.deliver_task(cx, &tid, &slot);
        }
        let queued: Vec<String> = self
            .tasks()
            .filter(|t| t.state == TaskState::Queued && self.ready(t, cx.now))
            .map(|t| t.id.clone())
            .collect();
        for tid in queued {
            let t = self.tasks[&tid].clone();
            let Some(target) = &t.target else { continue };
            // For a slot: when it runs, is idle, and has no task.
            if let Some(s) = self.slots.get(target).cloned() {
                let spent = s.had_task
                    && s.task.is_none()
                    && self.role_def(&s).is_some_and(|r| r.run == Run::Oneshot);
                if s.live() && !spent {
                    if self.takes_work(&s) {
                        self.bind(&tid, &s.id, cx.now);
                        self.deliver_task(cx, &tid, &s.id);
                    }
                    continue;
                }
                // The slot is gone: its role takes the task.
                if let Some(x) = self.tasks.get_mut(&tid) {
                    x.target = Some(s.role.clone());
                }
            }
            let role = self.tasks[&tid].target.clone().unwrap_or_default();
            let Some(def) = self
                .scope(t.board.as_deref())
                .and_then(|sp| sp.roles.get(&role))
                .cloned()
            else {
                continue;
            };
            let idle = self
                .slots
                .values()
                .find(|s| s.role == role && s.leader == t.board && self.takes_work(s))
                .map(|s| s.id.clone());
            if let Some(sid) = idle {
                self.bind(&tid, &sid, cx.now);
                self.deliver_task(cx, &tid, &sid);
                continue;
            }
            if def.run != Run::Oneshot {
                continue;
            }
            // A oneshot that already had its task is finishing: it does not count.
            let active = self
                .slots
                .values()
                .filter(|s| s.role == role && s.leader == t.board && s.active())
                .filter(|s| !(s.had_task && s.task.is_none()))
                .count() as u32;
            if active >= def.max {
                continue;
            }
            let sid = self.new_slot(t.board.as_deref(), &role);
            self.bind(&tid, &sid, cx.now);
            self.try_spawn(cx, &sid, &format!("A fresh {role} session for {tid}."));
        }
    }

    fn deliver_task(&mut self, cx: &mut Cx, tid: &str, slot: &str) {
        let Some(t) = self.tasks.get(tid).cloned() else {
            return;
        };
        if let Some(x) = self.tasks.get_mut(tid) {
            x.state = TaskState::Working;
        }
        if let Some(s) = self.slots.get_mut(slot) {
            s.busy = true;
            s.last_seen_ms = cx.now;
        }
        let text = self.task_text(tid);
        self.cmd(
            cx,
            Command::Deliver {
                to: Address::Slot { id: slot.into() },
                from: t.created_by,
                kind: MsgKind::Task,
                text,
            },
            &format!("{tid} goes to {slot}."),
        );
    }

    /// What a session reads for a task: what to do, and how to finish.
    pub fn task_text(&self, tid: &str) -> String {
        let Some(t) = self.tasks.get(tid) else {
            return String::new();
        };
        let mut out = format!("Task {tid}: {}", t.title);
        if !t.body.is_empty() {
            out.push_str(&format!("\n{}", t.body));
        }
        if !t.scope.is_empty() {
            out.push_str(&format!("\nScope: {}", t.scope.join(", ")));
        }
        if let Some(why) = &t.last_check {
            out.push_str(&format!(
                "\n\nAttempt {} of this task. The last one did not count:\n{why}",
                t.attempts + 1
            ));
        } else if t.attempts > 0 {
            out.push_str(&format!(
                "\nThis is attempt {}: an earlier session ended without finishing it.",
                t.attempts + 1
            ));
        }
        out.push_str(&format!(
            "\nWhen it is done, run: tend task done {tid} \"<one-line summary>\"\nIf you cannot do it, run: tend task blocked {tid} \"<reason>\""
        ));
        out
    }
}
