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
    /// A failed check was put down to another session in its shared worktree once (G5).
    #[serde(default)]
    pub interference_retried: bool,
    /// The task whose handoff made this one (G8), and the review round it is in.
    #[serde(default)]
    pub handoff_of: Option<String>,
    #[serde(default)]
    pub round: u32,
    /// A best-of task: its parts are the candidates, and it is the judging (G9).
    #[serde(default)]
    pub best_of: bool,
    /// Lines its worktree changed, from its passing check (G9).
    #[serde(default)]
    pub diff_lines: Option<u64>,
    /// A best-of candidate: picked by the judge, or not (G9).
    #[serde(default)]
    pub picked: Option<bool>,
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
        // best-of (G9): a task for the candidates fans out to one part each.
        let fan = match (&target, self.scope(board.as_deref())) {
            (Some(r), Some(sp))
                if sp.preset == crate::fleet::spec::Preset::BestOf && r == "candidate" =>
            {
                sp.roles.get(r).map(|d| d.max.max(1))
            }
            _ => None,
        };
        let id = self.next_task_id(parent.as_deref());
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
                interference_retried: false,
                handoff_of: None,
                round: 0,
                best_of: false,
                diff_lines: None,
                picked: None,
            },
        );
        if let Some(n) = fan {
            self.fan_out(cx, &id, n);
        }
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
        if t.best_of {
            return false;
        }
        t.depends_on.iter().all(|d| {
            self.tasks
                .get(d)
                .is_some_and(|x| x.state == TaskState::Done)
        }) && t.not_before.is_none_or(|nb| nb <= now)
    }

    /// The next id under `parent` (`t-N.k`), or at the top (`t-N`).
    fn next_task_id(&mut self, parent: Option<&str>) -> String {
        let n = self
            .task_numbers
            .entry(parent.unwrap_or_default().to_string())
            .or_insert(0);
        *n += 1;
        match parent {
            Some(p) => format!("{p}.{n}"),
            None => format!("t-{n}"),
        }
    }

    /// A task tend adds itself (a handoff, a review round, a best-of part), with the
    /// same creator, so its outcome still reaches whoever asked.
    fn add_follow_up(
        &mut self,
        from: &Task,
        parent: Option<String>,
        role: &str,
        title: String,
        body: String,
    ) -> String {
        let id = self.next_task_id(parent.as_deref());
        self.task_order.push(id.clone());
        self.tasks.insert(
            id.clone(),
            Task {
                id: id.clone(),
                title,
                body,
                depends_on: vec![],
                scope: from.scope.clone(),
                board: from.board.clone(),
                parent,
                created_by: from.created_by.clone(),
                target: Some(role.to_string()),
                slot: None,
                state: TaskState::Queued,
                attempts: 0,
                not_before: None,
                outcome: None,
                check: from.check.clone(),
                last_check: None,
                interference_retried: false,
                handoff_of: None,
                round: from.round,
                best_of: false,
                diff_lines: None,
                picked: None,
            },
        );
        id
    }

    /// The role that did a task: its session's, else the role it was for.
    fn role_of_task(&self, t: &Task) -> Option<String> {
        t.slot
            .as_ref()
            .and_then(|s| self.slots.get(s))
            .map(|s| s.role.clone())
            .or_else(|| t.target.clone().filter(|r| !self.slots.contains_key(r)))
    }

    /// G9: `n` candidate parts under a best-of task; the task waits for them.
    fn fan_out(&mut self, cx: &mut Cx, id: &str, n: u32) {
        let Some(t) = self.tasks.get(id).cloned() else {
            return;
        };
        let mut parts = Vec::new();
        for k in 1..=n {
            let body = format!(
                "You are candidate {k} of {n} on this task; the others work on it in their own worktrees. Do it your own way.\n{}",
                t.body
            );
            parts.push(self.add_follow_up(
                &t,
                Some(id.to_string()),
                "candidate",
                t.title.clone(),
                body,
            ));
        }
        if let Some(x) = self.tasks.get_mut(id) {
            x.best_of = true;
            x.state = TaskState::Queued;
            x.target = None;
            x.depends_on = parts.clone();
        }
        self.cmd(
            cx,
            Command::Notify {
                text: format!(
                    "tend fleet: {id} goes to {n} candidates ({}).",
                    parts.join(", ")
                ),
            },
            &format!("{id} is a best-of task: one part per candidate."),
        );
    }

    /// G8: what a done task hands on. A role with one handoff channel passes its work
    /// to the next role as a new task.
    fn after_done(&mut self, cx: &mut Cx, task: &str) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return;
        };
        if let Some(parent) = t
            .parent
            .clone()
            .filter(|p| self.tasks.get(p).is_some_and(|x| x.best_of))
        {
            return self.judge_if_ready(cx, &parent);
        }
        let Some(role) = self.role_of_task(&t) else {
            return;
        };
        let Some(spec) = self.scope(t.board.as_deref()) else {
            return;
        };
        let next: Vec<String> = spec
            .channels
            .iter()
            .filter(|c| {
                c.from == role
                    && c.kinds.contains(&MsgKind::Handoff)
                    && spec.roles.contains_key(&c.to)
            })
            .map(|c| c.to.clone())
            .collect();
        let [next] = next.as_slice() else {
            return;
        };
        let base = t.title.split(" — ").next().unwrap_or(&t.title).to_string();
        let summary = t.outcome.clone().unwrap_or_default();
        let body = format!(
            "Handed over from {role} ({}): {summary}\nThe work is in the shared worktree (or on branch tend/…/{}).\n{}",
            t.slot.clone().unwrap_or_default(),
            t.slot.clone().unwrap_or_default(),
            t.body
        );
        let id = self.add_follow_up(&t, t.parent.clone(), next, format!("{base} — {next}"), body);
        if let Some(x) = self.tasks.get_mut(&id) {
            x.handoff_of = Some(task.to_string());
        }
        self.cmd(
            cx,
            Command::Notify {
                text: format!("tend fleet: {task} ({role}) is handed to {next} as {id}."),
            },
            &format!("{role} hands its work to {next} (a handoff channel)."),
        );
    }

    /// G8: a review that was blocked with findings goes back to the author's role as a
    /// new task, up to the fleet's `max_rounds`.
    fn after_blocked(&mut self, cx: &mut Cx, task: &str) {
        let Some(t) = self.tasks.get(task).cloned() else {
            return;
        };
        let (Some(origin), Some(reviewer)) = (t.handoff_of.clone(), self.role_of_task(&t)) else {
            return;
        };
        let Some(o) = self.tasks.get(&origin).cloned() else {
            return;
        };
        let Some(author) = self.role_of_task(&o) else {
            return;
        };
        let Some(spec) = self.scope(t.board.as_deref()) else {
            return;
        };
        if !spec.allows(&reviewer, &author, MsgKind::Review) {
            return;
        }
        let max = spec.max_rounds;
        if t.round + 1 > max {
            return self.cmd(
                cx,
                Command::Notify {
                    text: format!("tend fleet: {task} was sent back {max} times; it stays blocked for you to decide."),
                },
                &format!("The review of {origin} reached max_rounds ({max})."),
            );
        }
        let base = o.title.split(" — ").next().unwrap_or(&o.title).to_string();
        let findings = t.outcome.clone().unwrap_or_default();
        let body = format!("The {reviewer} sent it back (round {} of {max}):\n{findings}\n\nYour earlier summary: {}", t.round + 1, o.outcome.clone().unwrap_or_default());
        let id = self.add_follow_up(
            &o,
            o.parent.clone(),
            &author,
            format!("{base} — {author}, round {}", t.round + 2),
            body,
        );
        if let Some(x) = self.tasks.get_mut(&id) {
            x.round = t.round + 1;
        }
        self.cmd(
            cx,
            Command::Notify {
                text: format!("tend fleet: {reviewer} sent {origin} back; {author} gets {id}."),
            },
            &format!("A review channel leads from {reviewer} back to {author}."),
        );
    }

    /// G9: when every candidate part has ended, the judge decides.
    fn judge_if_ready(&mut self, cx: &mut Cx, id: &str) {
        let Some(t) = self.tasks.get(id).cloned() else {
            return;
        };
        if !t.best_of || !matches!(t.state, TaskState::Queued | TaskState::Open) {
            return;
        }
        let parts: Vec<Task> = t
            .depends_on
            .iter()
            .filter_map(|p| self.tasks.get(p).cloned())
            .collect();
        let over = |x: &Task| {
            matches!(
                x.state,
                TaskState::Done | TaskState::Failed | TaskState::Blocked
            )
        };
        if !parts.iter().all(over) {
            return;
        }
        let done: Vec<&Task> = parts
            .iter()
            .filter(|x| x.state == TaskState::Done)
            .collect();
        if done.is_empty() {
            self.finish(cx, id, TaskState::Failed, "no candidate finished".into());
            let text = format!("{id} failed: no candidate finished.");
            return self.tell_creator(cx, &t.created_by, MsgKind::Report, &text);
        }
        let judge = self
            .scope(t.board.as_deref())
            .map_or(crate::fleet::spec::Judge::Rule, |s| s.judge);
        let lines: Vec<String> = done
            .iter()
            .map(|x| {
                format!(
                    "- {} ({}): {}{}",
                    x.id,
                    x.slot.clone().unwrap_or_default(),
                    x.outcome.clone().unwrap_or_default(),
                    x.diff_lines
                        .map(|n| format!(" [{n} lines changed]"))
                        .unwrap_or_default()
                )
            })
            .collect();
        match judge {
            crate::fleet::spec::Judge::Rule => {
                // The smallest passing diff; without sizes, the first to finish.
                let win = done
                    .iter()
                    .min_by_key(|x| x.diff_lines.unwrap_or(u64::MAX))
                    .map(|x| x.id.clone())
                    .unwrap_or_default();
                self.pick(id, &win);
                let note = format!(
                    "picked {win}: the smallest passing change of {}",
                    done.len()
                );
                self.finish(cx, id, TaskState::Done, note.clone());
                let text = format!("{id} is done: {note}.\n{}", lines.join("\n"));
                self.tell_creator(cx, &t.created_by, MsgKind::Report, &text);
            }
            crate::fleet::spec::Judge::Agent => {
                let finished: Vec<String> = done.iter().map(|x| x.id.clone()).collect();
                if let Some(x) = self.tasks.get_mut(id) {
                    x.best_of = false;
                    x.target = Some("judge".into());
                    x.depends_on = finished;
                    x.body = format!(
                        "{}\n\nThe candidates' results:\n{}\nPick one: name its task id (for example `{}`) in your `tend task done {id}` summary, with your reason.",
                        x.body,
                        lines.join("\n"),
                        done[0].id
                    );
                }
            }
            crate::fleet::spec::Judge::User => {
                self.finish(cx, id, TaskState::Review, "the candidates are done".into());
                let text = format!(
                    "{id}'s candidates are done:\n{}\nPick one with `tend task done {id} \"<task id>: <reason>\"`.",
                    lines.join("\n")
                );
                self.tell_creator(cx, &t.created_by, MsgKind::Question, &text);
            }
        }
    }

    /// Mark the best-of parts of `id`: `win` picked, the rest not.
    fn pick(&mut self, id: &str, win: &str) {
        let parts = self
            .tasks
            .get(id)
            .map(|t| t.depends_on.clone())
            .unwrap_or_default();
        for p in parts {
            if let Some(x) = self.tasks.get_mut(&p) {
                x.picked = Some(p == win);
            }
        }
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
    pub(super) fn check_for(&self, t: &Task) -> Option<String> {
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
        // A judge's or the user's pick of a best-of task names the winning part (G9).
        let parts: Vec<String> = self
            .tasks
            .get(task)
            .map(|t| t.depends_on.clone())
            .unwrap_or_default();
        if state == TaskState::Done {
            if let Some(win) = parts.iter().find(|p| {
                note.contains(p.as_str())
                    && self
                        .tasks
                        .get(*p)
                        .is_some_and(|x| x.parent.as_deref() == Some(task) && x.picked.is_none())
            }) {
                let win = win.clone();
                self.pick(task, &win);
            }
        }
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
        if matches!(state, TaskState::Failed | TaskState::Blocked) {
            self.block_dependents(cx, task);
        }
        match state {
            TaskState::Done => self.after_done(cx, task),
            TaskState::Blocked => self.after_blocked(cx, task),
            TaskState::Failed => {
                let parent = self.tasks.get(task).and_then(|t| t.parent.clone());
                if let Some(p) = parent.filter(|p| self.tasks.get(p).is_some_and(|x| x.best_of)) {
                    self.judge_if_ready(cx, &p);
                }
            }
            _ => {}
        }
    }

    /// Tasks waiting on one that failed or is blocked would wait forever: they are
    /// blocked too, and their creator hears why. (A best-of task judges its parts.)
    fn block_dependents(&mut self, cx: &mut Cx, task: &str) {
        let waiting: Vec<(String, Actor)> = self
            .tasks()
            .filter(|t| !t.best_of && t.depends_on.iter().any(|d| d == task))
            .filter(|t| matches!(t.state, TaskState::Open | TaskState::Queued))
            .map(|t| (t.id.clone(), t.created_by.clone()))
            .collect();
        let state = self.tasks.get(task).map_or(TaskState::Failed, |t| t.state);
        let how = format!("{:?}", state).to_lowercase();
        for (id, creator) in waiting {
            let why = format!("it depends on {task}, which {how}");
            self.finish(cx, &id, TaskState::Blocked, why.clone());
            let text =
                format!("{id} is blocked: {why}. Fix {task} and add {id} again, or drop it.");
            self.tell_creator(cx, &creator, MsgKind::Question, &text);
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
        // It asked and waits for the answer: it stays open (G2).
        if s.awaiting.is_some() && s.task.is_some() {
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
        // Another session at work in the same team worktree may have broken it (G5): the
        // first such failure goes back without costing an attempt.
        let team = t
            .slot
            .as_ref()
            .and_then(|s| self.slots.get(s))
            .and_then(|s| self.role_def(s))
            .and_then(|r| match &r.workspace {
                Workspace::Team { name } => Some(name.clone()),
                _ => None,
            });
        let interfered = !t.interference_retried
            && team.is_some_and(|team| {
                let ws = Workspace::Team { name: team };
                self.slots.values().any(|o| {
                    Some(&o.id) != t.slot.as_ref()
                        && o.live()
                        && o.task.is_some()
                        && self.role_def(o).is_some_and(|r| r.workspace == ws)
                })
            });
        let exit = exit.map_or("killed".to_string(), |c| format!("exit {c}"));
        let failure = format!(
            "The check `{check}` failed ({exit}):\n{}",
            crate::text::truncate_chars(excerpt.trim(), 2000)
        );
        if interfered {
            if let Some(x) = self.tasks.get_mut(task) {
                x.interference_retried = true;
            }
            let why = format!("{failure}\nAnother session was working in this shared worktree at the same time, and the failure may come from its change. Look before you fix. This attempt does not count.");
            return self.retry_with(cx, task, &who, why, false);
        }
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
        self.retry_with(cx, task, who, why, true)
    }

    /// `retry`; `counted` false when the attempt is not the session's fault.
    fn retry_with(&mut self, cx: &mut Cx, task: &str, who: &str, why: String, counted: bool) {
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
        let attempts = t.attempts + u32::from(counted);
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
            self.block_dependents(cx, &tid);
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
        let before: Vec<String> = t
            .depends_on
            .iter()
            .filter_map(|d| self.tasks.get(d))
            .filter(|d| d.parent.as_deref() != Some(tid))
            .map(|d| {
                format!(
                    "- {} ({}): {}",
                    d.id,
                    d.title,
                    d.outcome.clone().unwrap_or_default()
                )
            })
            .collect();
        if !before.is_empty() {
            out.push_str(&format!("\nDone before this:\n{}", before.join("\n")));
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
