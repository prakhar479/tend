//! Fleet files (F3b): roles, channels, and limits, filled in from a preset and checked.
//! The CLI reads the file's text into `SpecFile`; `resolve` turns it into a `Spec`.
//!
//! A role may lead a team of its own (`team`): the hierarchy. Inside a team, the name
//! `parent` means the session that leads it. Limits only narrow going down.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Deepest hierarchy, whatever the file says.
pub const MAX_DEPTH: u32 = 4;
/// Sessions at once when the file sets no limit. The CLI may lower it to fit memory
/// (F-S2: each Claude Code process takes about 300 MiB).
pub const DEFAULT_SESSIONS: u32 = 4;
/// A session with a turn in progress and no hook for this long is stuck.
pub const DEFAULT_STUCK_MINUTES: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    Solo,
    Peers,
    Lead,
    Pipeline,
    Review,
    BestOf,
    Specialists,
    Pool,
    Tree,
    Custom,
}

/// How a role's sessions run (design §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Run {
    /// Headless, long-lived; tend holds stdin and gives it turns.
    Stream,
    /// Headless, one task, then exit.
    Oneshot,
    /// The user's own terminal. tend never starts it.
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Workspace {
    /// One worktree and branch per worker slot (the default).
    WorktreePerWorker,
    /// One worktree for a named group of workers.
    Team { name: String },
    /// The user's checkout.
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgKind {
    Task,
    Report,
    Question,
    Answer,
    Review,
    Notice,
    Handoff,
}

/// Fleet commands a role's agent may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Verb {
    #[serde(rename = "task.add")]
    TaskAdd,
    #[serde(rename = "task.assign")]
    TaskAssign,
    #[serde(rename = "spawn")]
    Spawn,
    #[serde(rename = "merge.propose")]
    MergePropose,
    #[serde(rename = "msg")]
    Msg,
}

/// Who picks the winner in `best-of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Judge {
    /// The check passes, then the smallest diff.
    Rule,
    Agent,
    User,
}

/// A number, or text with a unit (`"2M"` tokens, `"45m"` of wall time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Amount {
    Int(u64),
    Text(String),
}

// The file as parsed. Every field is optional: the preset fills the rest.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecFile {
    pub preset: Option<Preset>,
    /// The command that decides whether a task is done (design §3.4).
    pub check: Option<String>,
    /// Add a supervisor that gets every role's notices (an overlay).
    pub supervisor: Option<bool>,
    pub max_rounds: Option<u32>,
    pub judge: Option<Judge>,
    pub stuck_minutes: Option<u64>,
    #[serde(default)]
    pub role: BTreeMap<String, RoleFile>,
    #[serde(default)]
    pub channel: Vec<ChannelFile>,
    pub limits: Option<LimitsFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleFile {
    pub agent: Option<String>,
    pub model: Option<String>,
    /// `"1..4"`, or one number.
    pub replicas: Option<Amount>,
    pub run: Option<Run>,
    /// `worktree-per-worker`, `shared`, or `team:<name>`.
    pub workspace: Option<String>,
    pub permission_mode: Option<String>,
    pub may_write: Option<Vec<String>>,
    pub may_not_write: Option<Vec<String>>,
    pub may: Option<Vec<Verb>>,
    pub budget: Option<BudgetFile>,
    pub brief: Option<String>,
    /// This role leads a team: a fleet inside the fleet.
    pub team: Option<Box<SpecFile>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetFile {
    pub tokens: Option<Amount>,
    pub wall: Option<Amount>,
    pub attempts: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelFile {
    pub from: String,
    pub to: String,
    pub kinds: Vec<MsgKind>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    pub sessions: Option<u32>,
    pub tokens: Option<Amount>,
    pub depth: Option<u32>,
}

// The resolved form.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub preset: Preset,
    pub check: Option<String>,
    pub roles: BTreeMap<String, Role>,
    pub channels: Vec<Channel>,
    pub limits: Limits,
    pub max_rounds: u32,
    pub judge: Judge,
    pub stuck_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub agent: String,
    pub model: Option<String>,
    pub min: u32,
    pub max: u32,
    pub run: Run,
    pub workspace: Workspace,
    pub permission_mode: Option<String>,
    pub may_write: Vec<String>,
    pub may_not_write: Vec<String>,
    pub may: BTreeSet<Verb>,
    pub budget: Budget,
    pub brief: String,
    pub team: Option<Box<Spec>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: Option<u64>,
    pub wall_ms: Option<u64>,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// A role, `user`, `parent` (inside a team), or `*`.
    pub from: String,
    pub to: String,
    pub kinds: BTreeSet<MsgKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub sessions: u32,
    pub tokens: Option<u64>,
    /// Levels of teams below the top, counting the top as 1.
    pub depth: u32,
}

impl Spec {
    /// Whether `kind` may go from `from` to `to` (role names, `user`, or `parent`).
    pub fn allows(&self, from: &str, to: &str, kind: MsgKind) -> bool {
        self.channels.iter().any(|c| {
            (c.from == from || c.from == "*")
                && (c.to == to || c.to == "*")
                && c.kinds.contains(&kind)
        })
    }
}

fn role(run: Run, min: u32, max: u32, may: &[Verb], brief: &str) -> Role {
    Role {
        agent: "claude".into(),
        model: None,
        min,
        max,
        run,
        workspace: Workspace::WorktreePerWorker,
        permission_mode: None,
        may_write: vec![],
        may_not_write: vec![],
        may: may.iter().copied().collect(),
        budget: Budget {
            attempts: 3,
            ..Budget::default()
        },
        brief: brief.into(),
        team: None,
    }
}

fn channel(from: &str, to: &str, kinds: &[MsgKind]) -> Channel {
    Channel {
        from: from.into(),
        to: to.into(),
        kinds: kinds.iter().copied().collect(),
    }
}

use MsgKind::*;
use Verb::*;

const LEAD_BRIEF: &str = "You lead a team of agents through tend. Split the work into tasks with `tend task add` (give each a `--check` that tests its part), give them out with `tend task assign`, and answer workers' questions. Each worker has its own worktree; tend brings a task's dependencies' work into it, and a finished part's work into yours. A task is done only when tend's check passes.";
const SUB_LEAD_BRIEF: &str = "You lead a part of the work, with workers of your own. Give them tasks with `tend task add` and `tend task assign`, and report to your parent with `tend msg @parent`.";
const WORKER_BRIEF: &str = "You are a worker in a tend fleet. Do the one task you were given, in this worktree. To ask your lead, run `tend msg @lead \"<question>\"` and end your turn: the answer comes as your next message. Finish with `tend task done <id> \"summary\"`, or `tend task blocked <id> \"reason\"`.";

/// The roles and channels a preset starts from. Inside a team (`as_team`), the lead is
/// the parent session, so presets drop their `lead` role and say `parent` instead.
fn preset(p: Preset, as_team: bool) -> (BTreeMap<String, Role>, Vec<Channel>) {
    let mut roles = BTreeMap::new();
    let mut chans = Vec::new();
    let lead = if as_team { "parent" } else { "lead" };
    let worker = || role(Run::Oneshot, 0, 4, &[Msg], WORKER_BRIEF);
    let lead_role = || {
        role(
            Run::Stream,
            1,
            1,
            &[TaskAdd, TaskAssign, Spawn, MergePropose, Msg],
            LEAD_BRIEF,
        )
    };
    let lead_channels = |chans: &mut Vec<Channel>, members: &[&str]| {
        for m in members {
            chans.push(channel(lead, m, &[Task, Answer, Review]));
            chans.push(channel(m, lead, &[Report, Question]));
        }
        if !as_team {
            chans.push(channel("user", lead, &[Task, Question, Answer, Notice]));
            chans.push(channel(lead, "user", &[Report, Question, Answer]));
        }
    };
    match p {
        Preset::Solo => {
            roles.insert(
                "agent".into(),
                role(
                    Run::Attached,
                    1,
                    1,
                    &[Msg],
                    "You work alone; tend records what you do.",
                ),
            );
        }
        Preset::Peers => {
            let mut peer = role(
                Run::Stream,
                2,
                4,
                &[Msg],
                "You are one of several peers in a tend fleet. Tell the others what you change with `tend msg @peer`.",
            );
            peer.workspace = Workspace::Shared;
            roles.insert("peer".into(), peer);
            chans.push(channel("peer", "peer", &[Notice, Question, Answer]));
            chans.push(channel("user", "peer", &[Task, Answer]));
            chans.push(channel("peer", "user", &[Report, Question]));
        }
        Preset::Lead | Preset::Specialists | Preset::Tree => {
            if !as_team {
                roles.insert("lead".into(), lead_role());
            }
            let members: Vec<&str> = match p {
                Preset::Specialists => {
                    let mut tester = worker();
                    tester.may_write = vec!["tests/**".into()];
                    tester.brief = "You write and fix tests only. Others implement.".into();
                    let mut implementer = worker();
                    implementer.may_not_write = vec!["tests/**".into()];
                    implementer.brief =
                        "You implement; you may not edit tests. Ask the lead if a test seems wrong."
                            .into();
                    roles.insert("tester".into(), tester);
                    roles.insert("implementer".into(), implementer);
                    vec!["tester", "implementer"]
                }
                Preset::Tree if !as_team => {
                    let (team_roles, team_chans) = preset(Preset::Lead, true);
                    let mut sub = lead_role();
                    sub.min = 2;
                    sub.max = 2;
                    sub.brief = SUB_LEAD_BRIEF.replace("@parent", "@lead");
                    sub.team = Some(Box::new(Spec {
                        preset: Preset::Lead,
                        check: None,
                        roles: team_roles,
                        channels: team_chans,
                        limits: Limits {
                            sessions: 2,
                            tokens: None,
                            depth: 1,
                        },
                        max_rounds: 3,
                        judge: Judge::Rule,
                        stuck_ms: DEFAULT_STUCK_MINUTES * 60_000,
                    }));
                    roles.insert("sub".into(), sub);
                    vec!["sub"]
                }
                _ => {
                    roles.insert("worker".into(), worker());
                    vec!["worker"]
                }
            };
            lead_channels(&mut chans, &members);
        }
        Preset::Pipeline => {
            let stages = ["plan", "implement", "verify"];
            for (i, s) in stages.iter().enumerate() {
                let mut r = worker();
                r.max = 1;
                r.workspace = Workspace::Team {
                    name: "pipeline".into(),
                };
                r.brief = format!("You are the {s} stage of a pipeline. When you finish, tend hands your summary and diff to the next stage.");
                roles.insert(s.to_string(), r);
                if let Some(next) = stages.get(i + 1) {
                    chans.push(channel(s, next, &[Handoff]));
                }
            }
            chans.push(channel("user", "plan", &[Task]));
            chans.push(channel("verify", "user", &[Report]));
            chans.push(channel("*", "user", &[Question]));
        }
        Preset::Review => {
            let mut author = worker();
            author.max = 1;
            author.workspace = Workspace::Team {
                name: "review".into(),
            };
            let mut reviewer = author.clone();
            author.brief = "You write the change. A reviewer reads it; fix what they find.".into();
            reviewer.brief = "You review the author's diff, audit, and check result. Send findings with `tend msg @author`, or approve.".into();
            roles.insert("author".into(), author);
            roles.insert("reviewer".into(), reviewer);
            chans.push(channel("author", "reviewer", &[Handoff]));
            chans.push(channel("reviewer", "author", &[Review]));
            chans.push(channel("user", "author", &[Task]));
            chans.push(channel("reviewer", "user", &[Report]));
        }
        Preset::BestOf => {
            let mut c = worker();
            c.min = 3;
            c.max = 3;
            c.brief = "You are one of several candidates on the same task. Do it your own way; a judge picks one result.".into();
            roles.insert("candidate".into(), c);
            let mut j = worker();
            j.max = 1;
            j.brief = "You compare the candidates' results and pick one, with a reason.".into();
            roles.insert("judge".into(), j);
            chans.push(channel("user", "candidate", &[Task]));
            chans.push(channel("candidate", "judge", &[Report]));
            chans.push(channel("judge", "user", &[Report]));
        }
        Preset::Pool => {
            roles.insert("worker".into(), worker());
            chans.push(channel("user", "worker", &[Task]));
            chans.push(channel("worker", "user", &[Report, Question]));
        }
        Preset::Custom => {}
    }
    (roles, chans)
}

/// Fill a fleet file in from its preset and check it. Errors are short sentences, each
/// naming the role or field.
pub fn resolve(file: &SpecFile) -> Result<Spec, Vec<String>> {
    let mut errors = Vec::new();
    let spec = resolve_at(file, 1, None, None, false, "", &mut errors);
    if errors.is_empty() {
        Ok(spec)
    } else {
        Err(errors)
    }
}

/// `level` is 1 at the top; `max_level` is the deepest level allowed, set at the top by
/// `limits.depth` and only lowered by a team's own.
fn resolve_at(
    file: &SpecFile,
    level: u32,
    max_level: Option<u32>,
    parent: Option<&Limits>,
    as_team: bool,
    at: &str,
    errors: &mut Vec<String>,
) -> Spec {
    let p = file.preset.unwrap_or(Preset::Custom);
    let (mut roles, mut channels) = preset(p, as_team);
    let has_team = |roles: &BTreeMap<String, Role>, file: &SpecFile| {
        roles.values().any(|r| r.team.is_some()) || file.role.values().any(|r| r.team.is_some())
    };
    let default_depth = if has_team(&roles, file) { 2 } else { 1 };
    let lf = file.limits.clone().unwrap_or_default();
    let mut limits = Limits {
        sessions: lf.sessions.unwrap_or(match parent {
            Some(pl) => pl.sessions,
            None => DEFAULT_SESSIONS,
        }),
        tokens: None,
        depth: lf.depth.unwrap_or(match parent {
            Some(pl) => pl.depth.saturating_sub(1).max(1),
            None => default_depth,
        }),
    };
    if let Some(t) = &lf.tokens {
        match tokens(t) {
            Some(v) => limits.tokens = Some(v),
            None => errors.push(format!(
                "{at}limits.tokens: `{t:?}` is not a token count like 20M."
            )),
        }
    }
    if file.stuck_minutes == Some(0) {
        errors.push(format!("{at}stuck_minutes must be at least 1."));
    }
    if limits.sessions == 0 {
        errors.push(format!("{at}limits.sessions must be at least 1."));
    }
    if level == 1 && limits.depth > MAX_DEPTH {
        errors.push(format!(
            "{at}limits.depth is {}; the most tend allows is {MAX_DEPTH}.",
            limits.depth
        ));
    }
    let own = (level - 1).saturating_add(limits.depth).min(MAX_DEPTH);
    let max_level = max_level.map_or(own, |m| m.min(own));
    if let Some(pl) = parent {
        if limits.sessions > pl.sessions {
            errors.push(format!(
                "{at}limits.sessions is {}, more than the {} its parent has.",
                limits.sessions, pl.sessions
            ));
        }
        if let (Some(c), Some(pt)) = (limits.tokens, pl.tokens) {
            if c > pt {
                errors.push(format!("{at}limits.tokens is more than its parent's {pt}."));
            }
        }
        if limits.tokens.is_none() {
            limits.tokens = pl.tokens;
        }
    }

    let mut added = Vec::new();
    for (name, rf) in &file.role {
        let here = format!("{at}role.{name}");
        if !valid_name(name) {
            errors.push(format!(
                "{here}: a role name uses lowercase letters, digits, and `-`, and is not user, parent, or *."
            ));
            continue;
        }
        let new = !roles.contains_key(name);
        if new {
            added.push(name.clone());
        }
        let r = roles
            .entry(name.clone())
            .or_insert_with(|| role(Run::Oneshot, 0, 1, &[Msg], WORKER_BRIEF));
        apply(r, rf, &here, errors);
        if let Some(team) = &rf.team {
            // A team's leader leads: it gets the lead's verbs and brief unless set.
            if rf.may.is_none() {
                r.may = [TaskAdd, TaskAssign, Spawn, MergePropose, Msg]
                    .into_iter()
                    .collect();
            }
            if new && rf.brief.is_none() {
                r.brief = SUB_LEAD_BRIEF.into();
            }
            if level + 1 > max_level {
                errors.push(format!(
                    "{here}.team is {} levels deep, but the fleet allows {max_level}.",
                    level + 1
                ));
            } else {
                let child = resolve_at(
                    team,
                    level + 1,
                    Some(max_level),
                    Some(&limits),
                    true,
                    &format!("{here}.team."),
                    errors,
                );
                r.team = Some(Box::new(child));
            }
        }
    }
    // In presets with a lead, a role the file adds is a member: it takes tasks from the
    // lead and reports to it, like the preset's own members.
    if matches!(p, Preset::Lead | Preset::Specialists | Preset::Tree) {
        let lead = if as_team { "parent" } else { "lead" };
        for m in &added {
            channels.push(channel(lead, m, &[Task, Answer, Review]));
            channels.push(channel(m, lead, &[Report, Question]));
        }
    }
    if file.supervisor == Some(true) && !roles.contains_key("supervisor") {
        let others: Vec<String> = roles.keys().cloned().collect();
        roles.insert(
            "supervisor".into(),
            role(
                Run::Stream,
                1,
                1,
                &[Msg],
                "You watch the fleet. tend sends you every loop, block, and stuck session. Nudge a session, or ask the user.",
            ),
        );
        for o in others {
            channels.push(channel(&o, "supervisor", &[Notice, Question]));
            channels.push(channel("supervisor", &o, &[Notice, Answer]));
        }
        channels.push(channel("supervisor", "user", &[Question, Report]));
    }
    for c in &file.channel {
        channels.push(Channel {
            from: c.from.clone(),
            to: c.to.clone(),
            kinds: c.kinds.iter().copied().collect(),
        });
    }

    check(&roles, &channels, &limits, as_team, at, errors);
    Spec {
        preset: p,
        check: file.check.clone(),
        roles,
        channels,
        limits,
        max_rounds: file.max_rounds.unwrap_or(3),
        judge: file.judge.unwrap_or(Judge::Rule),
        stuck_ms: file
            .stuck_minutes
            .unwrap_or(DEFAULT_STUCK_MINUTES)
            .saturating_mul(60_000),
    }
}

/// The file's fields over the preset's.
fn apply(r: &mut Role, f: &RoleFile, here: &str, errors: &mut Vec<String>) {
    if let Some(a) = &f.agent {
        r.agent = a.clone();
    }
    if f.model.is_some() {
        r.model = f.model.clone();
    }
    if let Some(run) = f.run {
        r.run = run;
    }
    if let Some(rep) = &f.replicas {
        match replicas(rep) {
            Some((min, max)) => (r.min, r.max) = (min, max),
            None => errors.push(format!(
                "{here}.replicas: `{rep:?}` is not a count like 2 or a range like 1..4."
            )),
        }
    }
    if let Some(w) = &f.workspace {
        match workspace(w) {
            Some(ws) => r.workspace = ws,
            None => errors.push(format!(
                "{here}.workspace: `{w}` is not worktree-per-worker, shared, or team:<name>."
            )),
        }
    }
    if f.permission_mode.is_some() {
        r.permission_mode = f.permission_mode.clone();
    }
    if let Some(v) = &f.may_write {
        r.may_write = v.clone();
    }
    if let Some(v) = &f.may_not_write {
        r.may_not_write = v.clone();
    }
    if let Some(v) = &f.may {
        r.may = v.iter().copied().collect();
    }
    if let Some(b) = &f.budget {
        if let Some(t) = &b.tokens {
            match tokens(t) {
                Some(v) => r.budget.tokens = Some(v),
                None => errors.push(format!(
                    "{here}.budget.tokens: `{t:?}` is not a token count like 2M."
                )),
            }
        }
        if let Some(w) = &b.wall {
            match duration_ms(w) {
                Some(v) => r.budget.wall_ms = Some(v),
                None => errors.push(format!(
                    "{here}.budget.wall: `{w:?}` is not a time like 45m or 2h."
                )),
            }
        }
        if let Some(a) = b.attempts {
            r.budget.attempts = a;
        }
    }
    if let Some(b) = &f.brief {
        r.brief = b.clone();
    }
}

/// Checks over the whole level, after the preset and the file are merged.
fn check(
    roles: &BTreeMap<String, Role>,
    channels: &[Channel],
    limits: &Limits,
    as_team: bool,
    at: &str,
    errors: &mut Vec<String>,
) {
    if roles.is_empty() {
        errors.push(format!(
            "{at}the fleet has no roles. Set a preset or add a role."
        ));
    }
    let mut start = 0;
    for (name, r) in roles {
        let here = format!("{at}role.{name}");
        if r.max == 0 || r.min > r.max {
            errors.push(format!("{here}.replicas: {}..{} is empty.", r.min, r.max));
        }
        if r.run == Run::Attached && r.max > 1 {
            errors.push(format!(
                "{here}: an attached role is your own terminal, so it has at most 1 session."
            ));
        }
        if r.team.is_some() && r.run != Run::Stream {
            errors.push(format!(
                "{here}: a role that leads a team must run as stream, to take its team's reports."
            ));
        }
        if r.may.contains(&Verb::Spawn) && r.run == Run::Oneshot {
            errors.push(format!(
                "{here}: a oneshot role cannot spawn; it ends after one task."
            ));
        }
        if let Some(g) = r.may_write.iter().find(|g| r.may_not_write.contains(g)) {
            errors.push(format!(
                "{here}: `{g}` is in both may_write and may_not_write."
            ));
        }
        if r.run == Run::Stream {
            start += r.min;
        }
    }
    if start > limits.sessions {
        errors.push(format!(
            "{at}the fleet starts {start} sessions, but limits.sessions is {}.",
            limits.sessions
        ));
    }
    let known =
        |n: &str| roles.contains_key(n) || n == "user" || n == "*" || (as_team && n == "parent");
    for c in channels {
        for end in [&c.from, &c.to] {
            if !known(end) {
                errors.push(format!(
                    "{at}channel {} → {}: no role named {end}.",
                    c.from, c.to
                ));
            }
        }
        if c.kinds.is_empty() {
            errors.push(format!(
                "{at}channel {} → {}: no message kinds.",
                c.from, c.to
            ));
        }
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && !matches!(n, "user" | "parent")
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `2`, `"2"`, or `"1..4"`.
fn replicas(a: &Amount) -> Option<(u32, u32)> {
    match a {
        Amount::Int(n) => {
            let n = u32::try_from(*n).ok()?;
            Some((n, n))
        }
        Amount::Text(t) => match t.split_once("..") {
            Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
            None => {
                let n = t.trim().parse().ok()?;
                Some((n, n))
            }
        },
    }
}

fn workspace(w: &str) -> Option<Workspace> {
    match w {
        "worktree-per-worker" => Some(Workspace::WorktreePerWorker),
        "shared" => Some(Workspace::Shared),
        _ => {
            let name = w.strip_prefix("team:")?;
            valid_name(name).then(|| Workspace::Team { name: name.into() })
        }
    }
}

/// `2000000`, `"2M"`, `"500k"`.
pub fn tokens(a: &Amount) -> Option<u64> {
    match a {
        Amount::Int(n) => Some(*n),
        Amount::Text(t) => {
            let t = t.trim();
            let (num, mul) = match t.chars().last()? {
                'k' | 'K' => (&t[..t.len() - 1], 1_000.0),
                'm' | 'M' => (&t[..t.len() - 1], 1_000_000.0),
                _ => (t, 1.0),
            };
            let v: f64 = num.trim().parse().ok()?;
            (v >= 0.0).then_some((v * mul) as u64)
        }
    }
}

/// `"90s"`, `"45m"`, `"2h"`, or a number of minutes.
pub fn duration_ms(a: &Amount) -> Option<u64> {
    match a {
        Amount::Int(m) => m.checked_mul(60_000),
        Amount::Text(t) => {
            let t = t.trim();
            let (num, mul) = match t.chars().last()? {
                's' => (&t[..t.len() - 1], 1_000),
                'm' => (&t[..t.len() - 1], 60_000),
                'h' => (&t[..t.len() - 1], 3_600_000),
                _ => (t, 60_000),
            };
            num.trim().parse::<u64>().ok()?.checked_mul(mul)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(p: Preset) -> Spec {
        resolve(&SpecFile {
            preset: Some(p),
            ..SpecFile::default()
        })
        .unwrap_or_else(|e| panic!("{p:?}: {e:?}"))
    }

    fn names(s: &Spec) -> Vec<&str> {
        s.roles.keys().map(String::as_str).collect()
    }

    /// (preset, its roles, edges it allows, edges it refuses)
    type PresetCase = (
        Preset,
        &'static [&'static str],
        &'static [(&'static str, &'static str, MsgKind)],
        &'static [(&'static str, &'static str, MsgKind)],
    );

    /// Each preset, as the design's table (§5) describes it.
    #[test]
    fn presets() {
        use Preset::*;
        let table: [PresetCase; 9] = [
            (Solo, &["agent"], &[], &[("agent", "user", Report)]),
            (
                Peers,
                &["peer"],
                &[("peer", "peer", Notice), ("user", "peer", Task)],
                &[("peer", "peer", Task)],
            ),
            (
                Lead,
                &["lead", "worker"],
                &[
                    ("lead", "worker", Task),
                    ("worker", "lead", Report),
                    ("user", "lead", Task),
                ],
                &[
                    ("worker", "worker", Task),
                    ("worker", "user", Report),
                    ("user", "worker", Task),
                ],
            ),
            (
                Pipeline,
                &["implement", "plan", "verify"],
                &[
                    ("plan", "implement", Handoff),
                    ("implement", "verify", Handoff),
                    ("verify", "user", Report),
                ],
                &[("verify", "plan", Handoff), ("plan", "verify", Handoff)],
            ),
            (
                Review,
                &["author", "reviewer"],
                &[
                    ("author", "reviewer", Handoff),
                    ("reviewer", "author", MsgKind::Review),
                ],
                &[("author", "user", Report)],
            ),
            (
                BestOf,
                &["candidate", "judge"],
                &[("candidate", "judge", Report), ("judge", "user", Report)],
                &[("candidate", "candidate", Report)],
            ),
            (
                Specialists,
                &["implementer", "lead", "tester"],
                &[("lead", "tester", Task), ("implementer", "lead", Question)],
                &[("tester", "implementer", Task)],
            ),
            (
                Pool,
                &["worker"],
                &[("user", "worker", Task), ("worker", "user", Report)],
                &[("worker", "worker", Task)],
            ),
            (
                Tree,
                &["lead", "sub"],
                &[("lead", "sub", Task), ("sub", "lead", Report)],
                &[("sub", "user", Report)],
            ),
        ];
        for (p, roles, yes, no) in table {
            let s = of(p);
            assert_eq!(names(&s), roles, "{p:?}");
            for (f, t, k) in yes {
                assert!(
                    s.allows(f, t, *k),
                    "{p:?}: {f} → {t} {k:?} should be allowed"
                );
            }
            for (f, t, k) in no {
                assert!(
                    !s.allows(f, t, *k),
                    "{p:?}: {f} → {t} {k:?} should be refused"
                );
            }
        }
    }

    #[test]
    fn preset_defaults_follow_the_owner_decisions() {
        let s = of(Preset::Lead);
        let (lead, worker) = (&s.roles["lead"], &s.roles["worker"]);
        assert_eq!(lead.run, Run::Stream);
        assert!(lead.may.contains(&Verb::Spawn));
        assert_eq!(worker.run, Run::Oneshot, "workers start fresh per task");
        assert_eq!(worker.workspace, Workspace::WorktreePerWorker);
        assert!(!worker.may.contains(&Verb::Spawn));
        let s = of(Preset::Specialists);
        assert_eq!(s.roles["implementer"].may_not_write, ["tests/**"]);
        assert_eq!(s.roles["tester"].may_write, ["tests/**"]);
    }

    #[test]
    fn tree_has_a_team_inside_with_narrower_limits() {
        let s = of(Preset::Tree);
        assert_eq!(s.limits.depth, 2);
        let team = s.roles["sub"].team.as_ref().expect("team");
        assert_eq!(names(team), ["worker"], "the sub-lead is the team's lead");
        assert!(team.allows("parent", "worker", MsgKind::Task));
        assert!(team.allows("worker", "parent", MsgKind::Question));
        assert!(
            !team.allows("worker", "user", MsgKind::Report),
            "escalation goes up"
        );
        assert!(team.limits.sessions <= s.limits.sessions);
    }

    fn file(toml_like: serde_json::Value) -> SpecFile {
        serde_json::from_value(toml_like).unwrap()
    }

    #[test]
    fn file_fields_override_the_preset() {
        let s = resolve(&file(serde_json::json!({
            "preset": "lead",
            "check": "make check",
            "role": {
                "lead": {"model": "opus"},
                "worker": {"model": "sonnet", "replicas": "1..3", "may_not_write": ["tests/**"],
                           "budget": {"tokens": "2M", "wall": "45m", "attempts": 2},
                           "workspace": "team:api"},
                "docs": {"run": "oneshot"}
            },
            "channel": [{"from": "lead", "to": "docs", "kinds": ["task"]}],
            "limits": {"sessions": 5, "tokens": "20M"}
        })))
        .unwrap();
        let w = &s.roles["worker"];
        assert_eq!((w.min, w.max), (1, 3));
        assert_eq!(w.model.as_deref(), Some("sonnet"));
        assert_eq!(w.budget.tokens, Some(2_000_000));
        assert_eq!(w.budget.wall_ms, Some(45 * 60_000));
        assert_eq!(w.budget.attempts, 2);
        assert_eq!(w.workspace, Workspace::Team { name: "api".into() });
        assert_eq!(s.roles["lead"].model.as_deref(), Some("opus"));
        assert_eq!(s.roles["lead"].run, Run::Stream, "kept from the preset");
        assert!(s.allows("lead", "docs", MsgKind::Task));
        assert_eq!(s.limits.tokens, Some(20_000_000));
        assert_eq!(s.check.as_deref(), Some("make check"));
    }

    #[test]
    fn a_role_that_leads_a_team_gets_the_lead_verbs() {
        let s = resolve(&file(serde_json::json!({
            "preset": "lead",
            "role": {
                "api": {"run": "stream", "team": {"preset": "lead"}},
                "ops": {"run": "stream", "may": ["msg"], "team": {"preset": "pool"}}
            }
        })))
        .unwrap();
        assert!(s.roles["api"].may.contains(&Verb::Spawn));
        assert!(
            s.roles["api"].brief.contains("@parent"),
            "{}",
            s.roles["api"].brief
        );
        assert!(
            !s.roles["ops"].may.contains(&Verb::Spawn),
            "a set `may` wins"
        );
        // Added roles are members of the lead's level, with its channels.
        assert!(s.allows("lead", "api", MsgKind::Task));
        assert!(s.allows("api", "lead", MsgKind::Report));
        let team = s.roles["api"].team.as_ref().unwrap();
        assert!(team.allows("parent", "worker", MsgKind::Task));
    }

    #[test]
    fn supervisor_overlay_hears_every_role() {
        let s = resolve(&file(
            serde_json::json!({"preset": "lead", "supervisor": true}),
        ))
        .unwrap();
        assert!(s.allows("worker", "supervisor", MsgKind::Notice));
        assert!(s.allows("lead", "supervisor", MsgKind::Question));
        assert!(s.allows("supervisor", "user", MsgKind::Question));
        assert_eq!(s.roles["supervisor"].run, Run::Stream);
    }

    fn errors(v: serde_json::Value) -> Vec<String> {
        resolve(&file(v)).expect_err("should not resolve")
    }

    #[test]
    fn bad_files_say_what_is_wrong() {
        let cases = [
            (serde_json::json!({}), "no roles"),
            (serde_json::json!({"preset": "custom"}), "no roles"),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"replicas": "4..1"}}}),
                "4..1 is empty",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"replicas": "lots"}}}),
                "not a count",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"Bad_Name": {}}}),
                "role name uses",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"user": {}}}),
                "role name uses",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"workspace": "elsewhere"}}}),
                "not worktree-per-worker",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"may": ["spawn"]}}}),
                "oneshot role cannot spawn",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"may_write": ["a"], "may_not_write": ["a"]}}}),
                "in both",
            ),
            (
                serde_json::json!({"preset": "solo", "role": {"agent": {"replicas": 2}}}),
                "at most 1 session",
            ),
            (
                serde_json::json!({"preset": "peers", "limits": {"sessions": 1}}),
                "starts 2 sessions",
            ),
            (
                serde_json::json!({"preset": "lead", "channel": [{"from": "lead", "to": "ghost", "kinds": ["task"]}]}),
                "no role named ghost",
            ),
            (
                serde_json::json!({"preset": "lead", "limits": {"tokens": "lots"}}),
                "limits.tokens",
            ),
            (
                serde_json::json!({"preset": "lead", "limits": {"depth": 9}}),
                "most tend allows is 4",
            ),
            (
                serde_json::json!({"preset": "lead", "role": {"worker": {"budget": {"wall": "soon"}}}}),
                "not a time",
            ),
        ];
        for (v, want) in cases {
            let e = errors(v.clone());
            assert!(e.iter().any(|m| m.contains(want)), "{v}: {e:?}");
        }
    }

    #[test]
    fn unknown_fields_are_refused_at_parse() {
        let r: Result<SpecFile, _> =
            serde_json::from_value(serde_json::json!({"preset": "lead", "rolez": {}}));
        assert!(r.is_err());
        let r: Result<SpecFile, _> = serde_json::from_value(serde_json::json!({"preset": "star"}));
        assert!(r.is_err());
    }

    #[test]
    fn teams_only_narrow_and_stay_within_depth() {
        // A team that asks for more sessions than its parent has.
        let e = errors(serde_json::json!({
            "preset": "lead", "limits": {"sessions": 4},
            "role": {"api": {"run": "stream", "team": {"preset": "lead", "limits": {"sessions": 6}}}}
        }));
        assert!(
            e.iter()
                .any(|m| m.contains("role.api.team.limits.sessions is 6, more than the 4")),
            "{e:?}"
        );
        // Two levels of teams with depth 2.
        let e = errors(serde_json::json!({
            "preset": "lead",
            "role": {"api": {"run": "stream", "team": {"preset": "lead",
                "role": {"db": {"run": "stream", "team": {"preset": "lead"}}}}}}
        }));
        assert!(e.iter().any(|m| m.contains("3 levels deep")), "{e:?}");
        // The same with depth 3 resolves.
        let ok = resolve(&file(serde_json::json!({
            "preset": "lead", "limits": {"depth": 3, "sessions": 4},
            "role": {"api": {"run": "stream", "team": {"preset": "lead",
                "role": {"db": {"run": "stream", "team": {"preset": "lead"}}}}}}
        })));
        assert!(ok.is_ok(), "{ok:?}");
        // A team leader that is not a stream session.
        let e = errors(serde_json::json!({
            "preset": "lead", "role": {"api": {"team": {"preset": "lead"}}}
        }));
        assert!(e.iter().any(|m| m.contains("must run as stream")), "{e:?}");
        // `parent` is a name only inside a team.
        let e = errors(serde_json::json!({
            "preset": "lead", "channel": [{"from": "parent", "to": "worker", "kinds": ["task"]}]
        }));
        assert!(
            e.iter().any(|m| m.contains("no role named parent")),
            "{e:?}"
        );
    }

    #[test]
    fn amounts() {
        assert_eq!(tokens(&Amount::Text("2M".into())), Some(2_000_000));
        assert_eq!(tokens(&Amount::Text("1.5m".into())), Some(1_500_000));
        assert_eq!(tokens(&Amount::Text("500k".into())), Some(500_000));
        assert_eq!(tokens(&Amount::Int(7)), Some(7));
        assert_eq!(tokens(&Amount::Text("-1".into())), None);
        assert_eq!(duration_ms(&Amount::Text("90s".into())), Some(90_000));
        assert_eq!(duration_ms(&Amount::Text("2h".into())), Some(7_200_000));
        assert_eq!(duration_ms(&Amount::Int(5)), Some(300_000));
        assert_eq!(replicas(&Amount::Text("1..4".into())), Some((1, 4)));
        assert_eq!(replicas(&Amount::Int(3)), Some((3, 3)));
    }

    #[test]
    fn huge_durations_are_refused_not_overflowed() {
        assert_eq!(duration_ms(&Amount::Int(u64::MAX)), None);
        assert_eq!(
            duration_ms(&Amount::Text("18446744073709551615h".into())),
            None
        );
        assert_eq!(duration_ms(&Amount::Text("2h".into())), Some(7_200_000));
    }

    #[test]
    fn a_huge_team_depth_is_an_error_not_a_panic() {
        let f: SpecFile = serde_json::from_value(serde_json::json!({
            "role": {"sub": {"run": "stream", "team": {
                "role": {"w": {}}, "limits": {"depth": 4294967295u64}
            }}}
        }))
        .unwrap();
        let _ = resolve(&f);
    }

    #[test]
    fn stuck_minutes_must_be_at_least_one() {
        let f: SpecFile = serde_json::from_value(serde_json::json!({
            "preset": "pool", "stuck_minutes": 0
        }))
        .unwrap();
        let e = resolve(&f).unwrap_err();
        assert!(e.iter().any(|m| m.contains("stuck_minutes")), "{e:?}");
    }
}
