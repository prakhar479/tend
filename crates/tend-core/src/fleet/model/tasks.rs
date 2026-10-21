//! The task board (F5a). A task lives on one board: the top (`t-N`), or the team of the
//! slot that leads it. A task added by a slot that is working on a task is part of that
//! task (`t-N.k`), and the parent is done only when its parts are.
//!
//! A queued task is for a role or a slot. `dispatch` gives a ready one (its dependencies
//! done) to an idle stream session of its role, or starts a fresh oneshot session for it.
//! A session that ends without `done` or `blocked` costs the task an attempt; past the
//! role's budget the task fails, and its creator is told.

use super::*;

/// A task as `tend task add` gives it.
pub struct NewTask {
    pub title: String,
    pub body: String,
    pub depends_on: Vec<String>,
    pub scope: Vec<String>,
    pub role: Option<String>,
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
        let mine = match &by {
            Actor::Slot { id } => t.slot.as_deref() == Some(id.as_str()),
            _ => true,
        };
        if !mine {
            return self.refuse(cx, &by, &format!("{task} is not this session's task."));
        }
        if !matches!(
            t.state,
            TaskState::Working | TaskState::Bound | TaskState::Queued | TaskState::Open
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
        let who = t.slot.clone().unwrap_or_else(|| "the user".into());
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
        if let Some(x) = self.tasks.get_mut(task) {
            x.state = state;
            x.outcome = Some(match outcome {
                Ok(s) | Err(s) => s,
            });
        }
        if let Some(sid) = &t.slot {
            if let Some(s) = self.slots.get_mut(sid) {
                s.task = None;
            }
        }
        self.tell_creator(cx, &t.created_by, kind, &text);
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
        if !self.up {
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
        if t.attempts > 0 {
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
