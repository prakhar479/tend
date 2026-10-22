//! The fleet hub (F3a): one process per fleet. It owns the fleet model (tend-core
//! `fleet::model`), records every event and command in the fleet's ledger, and runs the
//! commands: it starts sessions under `tend claude` in stream-json mode, holds their
//! stdin, watches their output and exits, and makes and keeps their worktrees.
//!
//! Layout: <home>/fleets/<id>/
//!   meta.json         the resolved fleet file, repo, base commit, mode
//!   ledger.jsonl      fleet records (events and commands)
//!   inbox.jsonl       what the fleet tells the user
//!   slots/<slot>.jsonl, .err   each slot's agent output
//!   worktrees/<slot>  each slot's worktree (branch tend/<id>/<slot>)
//!   hub.log, tendd.sock

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tend_core::fleet::model::{
    Actor, Address, Command as Cmd, FleetBody, FleetEvent, FleetRecord, Model, SlotState,
};
use tend_core::fleet::spec::{self, Preset, Run, Spec, SpecFile, Workspace};
use tend_core::Mode;

use crate::ipc::Reply;
use crate::paths::{self, now_ms, socket_path};

/// The hub sends the model a Tick this often.
const TICK: Duration = Duration::from_secs(2);
/// Activity events per slot, at most one this often (turn ends always go).
const ACTIVITY_EVERY_MS: u64 = 1_000;
/// After `stop`: the session's stdin is closed; then SIGTERM, then SIGKILL.
const TERM_AFTER: Duration = Duration::from_secs(5);
const KILL_AFTER: Duration = Duration::from_secs(10);
/// Memory one agent session takes (F-S2: ~300 MiB), with room.
const AGENT_BYTES: u64 = 350 << 20;

pub fn fleets_dir() -> PathBuf {
    paths::home().join("fleets")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// The worktree `up` ran in; worktrees branch from `base` there. None outside git.
    pub repo: Option<String>,
    pub base: Option<String>,
    pub cwd: String,
    pub mode: Mode,
    pub created_ms: u64,
    pub spec: Spec,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubRequest {
    Ping,
    Status,
    Stop {
        slot: String,
    },
    Down,
    Spawn {
        role: String,
    },
    Scale {
        role: String,
        n: u32,
    },
    Pause,
    Resume,
    /// A session in the user's terminal joins (F4): `pid` is its launcher's, so its
    /// tools' requests are told apart like a spawned session's.
    Join {
        role: Option<String>,
        name: Option<String>,
        pid: i32,
        cwd: String,
    },
    Leave {
        slot: String,
    },
    /// `tend msg`: `to` is `user`, `@role`, or a slot; no `kind` means the first the
    /// channel allows.
    Msg {
        to: String,
        kind: Option<String>,
        text: String,
        #[serde(default)]
        claimed_slot: Option<String>,
    },
    /// `tend inbox`: the caller's messages (a session's), or the fleet's (the user's).
    Inbox {
        #[serde(default)]
        claimed_slot: Option<String>,
    },
    /// A `tend task` verb (F5a). `claimed_slot` is `TEND_FLEET_SLOT`; it counts only
    /// where the hub cannot see the caller's process tree.
    Task {
        verb: TaskVerb,
        #[serde(default)]
        claimed_slot: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum TaskVerb {
    Add {
        title: String,
        body: String,
        depends_on: Vec<String>,
        scope: Vec<String>,
        role: Option<String>,
        #[serde(default)]
        check: Option<String>,
    },
    Assign {
        task: String,
        to: String,
    },
    Claim {
        task: Option<String>,
    },
    Done {
        task: String,
        summary: String,
    },
    Blocked {
        task: String,
        reason: String,
    },
    Reject {
        task: String,
        reason: String,
    },
    List,
}

// --- tend fleet up ---------------------------------------------------------------

/// Read a fleet file (TOML), or take a preset alone, and resolve it.
pub fn load_spec(file: Option<&Path>, preset: Option<Preset>) -> Result<(Spec, SpecFile)> {
    let mut f: SpecFile = match file {
        Some(p) => {
            let text = fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            toml::from_str(&text).map_err(|e| anyhow!("{}: {}", p.display(), e.message()))?
        }
        None => SpecFile::default(),
    };
    if preset.is_some() {
        f.preset = preset;
    }
    if f.preset.is_none() && f.role.is_empty() {
        bail!("give a fleet file or `--preset`, for example `tend fleet up --preset lead`");
    }
    let spec = spec::resolve(&f).map_err(|errs| {
        anyhow!(
            "the fleet file has {} problem{}:\n  {}",
            errs.len(),
            if errs.len() == 1 { "" } else { "s" },
            errs.join("\n  ")
        )
    })?;
    Ok((spec, f))
}

/// Sessions that fit in free memory (Linux), at least what the fleet starts with.
fn fit_sessions(spec: &Spec) -> Option<u32> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = text
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let fit = (kb * 1024 / AGENT_BYTES).max(1) as u32;
    let start: u32 = spec
        .roles
        .values()
        .filter(|r| r.run == Run::Stream)
        .map(|r| r.min)
        .sum();
    Some(fit.max(start))
}

/// Start a fleet: write its dir, start its hub, wait for it. Returns the fleet id.
pub fn up(file: Option<&Path>, preset: Option<Preset>, mode: Mode) -> Result<String> {
    let (mut spec, f) = load_spec(file, preset)?;
    let mut notes = Vec::new();
    if f.limits.as_ref().and_then(|l| l.sessions).is_none() {
        if let Some(fit) = fit_sessions(&spec) {
            if fit < spec.limits.sessions {
                notes.push(format!(
                    "Free memory fits about {fit} agent sessions, so the fleet runs at most {fit} at once (set limits.sessions to change it)."
                ));
                spec.limits.sessions = fit;
            }
        }
    }
    let cwd = std::env::current_dir().context("current directory")?;
    let (repo, base) = match git_out(&cwd, &["rev-parse", "--show-toplevel"]) {
        Some(top) => (Some(top), git_out(&cwd, &["rev-parse", "HEAD"])),
        None => (None, None),
    };
    if repo.is_none() {
        notes.push("This is not a git repo: every session works in this directory.".into());
    } else if base.is_none() {
        notes.push("The repo has no commit yet: every session works in this checkout.".into());
    }
    let id = paths::new_session_id();
    let dir = fleets_dir().join(&id);
    paths::ensure_private_dir(&fleets_dir())?;
    paths::ensure_private_dir(&dir)?;
    let meta = Meta {
        id: id.clone(),
        repo,
        base,
        cwd: cwd.display().to_string(),
        mode,
        created_ms: now_ms(),
        spec,
    };
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    for n in notes {
        inbox(&dir, &n);
    }
    let exe = std::env::current_exe().context("find tend executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hub.log"))?;
    Command::new(exe)
        .args(["fleet", "hub", "--dir"])
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("start the fleet hub")?;
    for _ in 0..200 {
        if send(&dir, &HubRequest::Ping).is_ok() {
            return Ok(id);
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "the fleet hub did not start; see {}",
        dir.join("hub.log").display()
    )
}

// --- The hub process ---------------------------------------------------------------

enum Msg {
    Event(FleetEvent),
    /// The slot's turn ended (a `result` line); close a oneshot's stdin.
    TurnEnded(String),
    /// A request, and the pid of the process that sent it (None: not known here).
    Request(HubRequest, UnixStream, Option<i32>),
}

struct Proc {
    pid: i32,
    /// Lines for the session's stdin, written by its own thread: a session that stops
    /// reading must never block the hub. Dropping it closes the stdin.
    stdin: Option<Sender<String>>,
    turns: u32,
    exited: Arc<AtomicBool>,
    worktree: PathBuf,
}

struct Hub {
    dir: PathBuf,
    meta: Meta,
    model: Model,
    ledger: File,
    procs: BTreeMap<String, Proc>,
    tx: Sender<Msg>,
    /// Events the hub itself caused while running commands (a spawn's start).
    queue: VecDeque<FleetEvent>,
    down_since: Option<u64>,
    /// Each slot's working directory, kept after its session ends (checks run there).
    worktrees: BTreeMap<String, PathBuf>,
    /// The worktree's tree when each task was delivered, for the audit at check time
    /// (only with `experimental_audit = true`): task → (root, tree).
    bases: BTreeMap<String, (PathBuf, String)>,
    /// Checks run one at a time: a team's sessions may share a worktree.
    checking: Arc<std::sync::Mutex<()>>,
    audit: bool,
    /// What claims.json holds now.
    claims_text: Option<String>,
    /// Slots of sessions in the user's terminal (F4): their launcher pid is watched.
    attached: std::collections::BTreeSet<String>,
    /// Each slot's last session id: a stream slot that starts again resumes it (G1).
    last_session: BTreeMap<String, String>,
    /// Slots whose worktree already got its dependencies' work.
    integrated: std::collections::BTreeSet<String>,
    /// Done parts whose work went up to their parent's worktree.
    lifted: std::collections::BTreeSet<String>,
}

pub fn run_hub(dir: &Path, resume: bool) -> Result<()> {
    let meta: Meta = serde_json::from_str(&fs::read_to_string(dir.join("meta.json"))?)?;
    let sock = socket_path(dir);
    if let Some(parent) = sock.parent() {
        paths::ensure_private_dir(parent)?;
    }
    let tmp = sock.with_file_name(format!("{}.t", std::process::id() % 10_000_000));
    let _ = fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
    fs::rename(&tmp, &sock)?;
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        thread::spawn(move || listen(listener, tx));
    }
    let ledger = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ledger.jsonl"))?;
    let mut hub = Hub {
        dir: dir.to_path_buf(),
        model: Model::new(meta.spec.clone()),
        meta,
        ledger,
        procs: BTreeMap::new(),
        tx,
        queue: VecDeque::new(),
        down_since: None,
        worktrees: BTreeMap::new(),
        bases: BTreeMap::new(),
        checking: Arc::new(std::sync::Mutex::new(())),
        audit: crate::config::load().experimental_audit,
        claims_text: None,
        attached: std::collections::BTreeSet::new(),
        last_session: BTreeMap::new(),
        integrated: std::collections::BTreeSet::new(),
        lifted: std::collections::BTreeSet::new(),
    };
    let _ = fs::write(dir.join("hub.pid"), std::process::id().to_string());
    if resume {
        // G1: the state the ledger describes, then the restart. The replayed records are
        // in the ledger already; only what comes after is written.
        for r in read_ledger(dir) {
            if let FleetBody::Event { event } = r.body {
                if let FleetEvent::Started { slot, session } = &event {
                    hub.last_session.insert(slot.clone(), session.clone());
                }
                hub.model.step(event, r.ts_ms);
            }
        }
        let slots: Vec<String> = hub.model.slots().keys().cloned().collect();
        for slot in slots {
            if let Some(dir) = hub.slot_dir(&slot) {
                hub.worktrees.insert(slot, dir);
            }
        }
        // Done parts went up before the hub stopped; their starts are gone with it.
        let done: Vec<String> = hub
            .model
            .tasks()
            .filter(|t| t.state == tend_core::fleet::model::TaskState::Done)
            .map(|t| t.id.clone())
            .collect();
        hub.lifted.extend(done);
        hub.integrated.extend(hub.model.slots().keys().cloned());
        // Sessions of the old hub may still finish a turn: stop them before new ones start.
        stop_old_sessions(dir);
        eprintln!("tend fleet hub {}: restarted", hub.meta.id);
        hub.apply(FleetEvent::HubRestarted);
    } else {
        eprintln!("tend fleet hub {}: up", hub.meta.id);
        hub.apply(FleetEvent::Up);
    }
    hub.serve(rx);
    let _ = fs::remove_file(&sock);
    eprintln!("tend fleet hub {}: down", hub.meta.id);
    Ok(())
}

fn listen(listener: UnixListener, tx: Sender<Msg>) {
    for conn in listener.incoming().flatten() {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        let Ok(reader) = conn.try_clone() else {
            continue;
        };
        if BufReader::new(reader)
            .take(1 << 20)
            .read_line(&mut line)
            .is_err()
            || line.is_empty()
        {
            continue;
        }
        match serde_json::from_str::<HubRequest>(&line) {
            Ok(HubRequest::Ping) => reply(conn, "pong"),
            Ok(req) => {
                let pid = peer_pid(&conn);
                if tx.send(Msg::Request(req, conn, pid)).is_err() {
                    return;
                }
            }
            Err(e) => reply(conn, &format!("bad request: {e}")),
        }
    }
}

fn reply(mut conn: UnixStream, message: &str) {
    let r = Reply {
        output: None,
        message: Some(message.to_string()),
    };
    if let Ok(mut line) = serde_json::to_string(&r) {
        line.push('\n');
        let _ = conn.write_all(line.as_bytes());
    }
}

impl Hub {
    fn serve(&mut self, rx: Receiver<Msg>) {
        // Tick on elapsed time: a busy hub (status polls, output) must still tick.
        let mut last_tick = now_ms();
        let tick_ms = TICK.as_millis() as u64;
        loop {
            let wait = tick_ms.saturating_sub(now_ms().saturating_sub(last_tick));
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(Msg::Event(e)) => {
                    self.apply(e);
                }
                Ok(Msg::TurnEnded(slot)) => {
                    // The model says when a oneshot ends (`release`): not while its
                    // task's check runs, so a failure can come back to it (F5b).
                    if let Some(p) = self.procs.get_mut(&slot) {
                        p.turns += 1;
                    }
                    self.apply(FleetEvent::Activity {
                        slot,
                        turn_ended: true,
                    });
                }
                Ok(Msg::Request(req, conn, pid)) => {
                    let text = self.request(req, pid);
                    reply(conn, &text);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if now_ms().saturating_sub(last_tick) >= tick_ms {
                last_tick = now_ms();
                self.apply(FleetEvent::Tick);
                // A terminal session that went away without `leave` (F4).
                let gone: Vec<String> = self
                    .attached
                    .iter()
                    .filter(|s| {
                        self.procs
                            .get(*s)
                            .is_none_or(|p| unsafe { libc::kill(p.pid, 0) } != 0)
                    })
                    .cloned()
                    .collect();
                for slot in gone {
                    self.attached.remove(&slot);
                    self.apply(FleetEvent::Exited { slot, code: None });
                }
            }
            if let Some(since) = self.down_since {
                let live = self
                    .model
                    .slots()
                    .values()
                    .any(|s| matches!(s.state, SlotState::Starting | SlotState::Running));
                if !live {
                    return;
                }
                // Sessions that ignore every signal: do not wait forever.
                if now_ms().saturating_sub(since) > 30_000 {
                    for p in self.procs.values() {
                        kill(-p.pid, libc::SIGKILL);
                    }
                    return;
                }
            }
        }
    }

    /// Who sent a request: the slot whose session's process tree it came from, or the
    /// user. Without the caller's pid (an OS where the hub cannot see it), the slot the
    /// caller names.
    fn actor(&self, pid: Option<i32>, claimed: Option<&str>) -> Actor {
        match pid {
            Some(pid) => {
                let tree = ancestors(pid);
                self.procs
                    .iter()
                    .find(|(_, p)| tree.contains(&p.pid))
                    .map_or(Actor::User, |(id, _)| Actor::Slot { id: id.clone() })
            }
            None => match claimed {
                Some(id) if self.procs.contains_key(id) => Actor::Slot { id: id.into() },
                _ => Actor::User,
            },
        }
    }

    fn request(&mut self, req: HubRequest, pid: Option<i32>) -> String {
        let claimed = match &req {
            HubRequest::Task { claimed_slot, .. }
            | HubRequest::Msg { claimed_slot, .. }
            | HubRequest::Inbox { claimed_slot } => claimed_slot.clone(),
            _ => None,
        };
        let by = self.actor(pid, claimed.as_deref());
        let user = by == Actor::User;
        let event = match req {
            HubRequest::Ping => return "pong".into(),
            HubRequest::Status => return self.status(),
            // Fleet control is the user's; a session asks through its role's verbs.
            HubRequest::Down
            | HubRequest::Scale { .. }
            | HubRequest::Pause
            | HubRequest::Resume
                if !user =>
            {
                return "tend: refused. Fleet control is the user's.".into();
            }
            HubRequest::Stop { slot } => FleetEvent::StopRequest { by, slot },
            HubRequest::Down => FleetEvent::Down,
            HubRequest::Spawn { role } => FleetEvent::SpawnRequest { by, role },
            HubRequest::Scale { role, n } => FleetEvent::Scale { role, n },
            HubRequest::Pause => FleetEvent::Pause,
            HubRequest::Resume => FleetEvent::Resume,
            HubRequest::Join { .. } if !user => {
                return "tend: refused. A fleet session cannot join another.".into();
            }
            HubRequest::Join {
                role,
                name,
                pid,
                cwd,
            } => return self.join(role, name, pid, cwd),
            HubRequest::Leave { slot } => {
                // A session may end only its own watch (dogfood review): the user's
                // launcher, or the slot's own process tree.
                if matches!(&by, Actor::Slot { id } if *id != slot) {
                    return format!("tend: refused. Only {slot} or the user can end its slot.");
                }
                if !self.attached.remove(&slot) {
                    return format!("tend: refused. {slot} did not join from a terminal.");
                }
                FleetEvent::Exited {
                    slot,
                    code: Some(0),
                }
            }
            HubRequest::Inbox { .. } => return self.inbox_of(&by),
            HubRequest::Msg { to, kind, text, .. } => {
                let to = match to.as_str() {
                    "user" => Address::User,
                    t => match (t.strip_prefix('@'), t.strip_prefix('#')) {
                        (Some(r), _) => Address::Role { name: r.into() },
                        (_, Some(team)) => Address::Team { name: team.into() },
                        _ => Address::Slot { id: t.into() },
                    },
                };
                let kind = match kind {
                    Some(k) => match serde_json::from_value(serde_json::Value::String(k.clone())) {
                        Ok(k) => k,
                        Err(_) => return format!("tend: refused. No message kind `{k}` (question, answer, report, notice, review, handoff)."),
                    },
                    None => match self.model.default_kind(&by, &to) {
                        Some(k) => k,
                        None => return "tend: refused. No channel of this fleet lets this sender reach that receiver.".into(),
                    },
                };
                FleetEvent::Message {
                    from: by,
                    to,
                    kind,
                    text,
                }
            }
            HubRequest::Task { verb, .. } => match verb {
                TaskVerb::List => return render_tasks(&self.model),
                TaskVerb::Add {
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                    check,
                } => FleetEvent::TaskAdd {
                    by,
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                    check,
                },
                TaskVerb::Assign { task, to } => FleetEvent::TaskAssign { by, task, to },
                TaskVerb::Claim { task } => FleetEvent::TaskClaim { by, task },
                TaskVerb::Done { task, summary } => FleetEvent::TaskDone { by, task, summary },
                TaskVerb::Blocked { task, reason } => FleetEvent::TaskBlocked { by, task, reason },
                TaskVerb::Reject { task, reason } => FleetEvent::TaskReject { by, task, reason },
            },
        };
        let recs = self.apply(event);
        // The answer to the asker, if the model gave one; else what happened.
        let answers: Vec<String> = recs
            .iter()
            .filter_map(|r| match &r.body {
                FleetBody::Command {
                    command: Cmd::Answer { text, .. },
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        if !answers.is_empty() {
            return answers.join("\n");
        }
        let said: Vec<String> = recs
            .iter()
            .filter_map(|r| match &r.body {
                FleetBody::Command {
                    command: Cmd::Notify { text },
                    ..
                } => Some(text.clone()),
                FleetBody::Command { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        if said.is_empty() {
            "Nothing to do.".into()
        } else {
            said.join("\n")
        }
    }

    /// Step the model with `event` and with what running its commands caused; record
    /// and run every command. Returns the records of `event` itself.
    fn apply(&mut self, event: FleetEvent) -> Vec<FleetRecord> {
        if matches!(event, FleetEvent::Down) && self.down_since.is_none() {
            self.down_since = Some(now_ms());
        }
        let mut first = None;
        self.queue.push_back(event);
        while let Some(e) = self.queue.pop_front() {
            if let FleetEvent::Exited { slot, .. } = &e {
                self.procs.remove(slot);
            }
            let recs = self.model.step(e, now_ms());
            for r in &recs {
                if let Ok(line) = serde_json::to_string(r) {
                    let _ = writeln!(self.ledger, "{line}");
                }
            }
            // A done part's work goes up before its lead hears "is done": a lead that
            // looks at once must find it (the e2e test hid this with a sleep).
            self.bring_up_done_parts();
            for r in &recs {
                if let FleetBody::Command { command, .. } = &r.body {
                    self.run(command);
                }
            }
            if first.is_none() {
                first = Some(recs);
            }
        }
        self.write_claims();
        self.bring_up_done_parts();
        first.unwrap_or_default()
    }

    fn run(&mut self, c: &Cmd) {
        match c {
            Cmd::Spawn {
                slot,
                workspace,
                model,
                permission_mode,
                brief,
                ..
            } => match self.spawn(slot, workspace, model.as_deref(), permission_mode.as_deref(), brief) {
                Ok(session) => {
                    self.last_session.insert(slot.clone(), session.clone());
                    self.queue.push_back(FleetEvent::Started {
                        slot: slot.clone(),
                        session,
                    })
                }
                Err(e) => {
                    eprintln!("tend fleet hub: start {slot}: {e:#}");
                    inbox(&self.dir, &format!("Could not start {slot}: {e:#}"));
                    self.queue.push_back(FleetEvent::Exited {
                        slot: slot.clone(),
                        code: Some(127),
                    });
                }
            },
            // A terminal session is the user's: the fleet lets go of it, it goes on alone.
            Cmd::Stop { slot } if self.attached.contains(slot) => {
                self.attached.remove(slot);
                inbox(&self.dir, &format!("{slot} left the fleet; its terminal session goes on without it."));
                self.queue.push_back(FleetEvent::Exited {
                    slot: slot.clone(),
                    code: Some(0),
                });
            }
            Cmd::Stop { slot } => {
                if let Some(p) = self.procs.get_mut(slot) {
                    // A stream session ends cleanly when its stdin closes (F-S1).
                    p.stdin = None;
                    let (pid, exited) = (p.pid, Arc::clone(&p.exited));
                    thread::spawn(move || {
                        for (wait, sig) in [(TERM_AFTER, libc::SIGTERM), (KILL_AFTER, libc::SIGKILL)] {
                            thread::sleep(wait);
                            if exited.load(Ordering::SeqCst) {
                                return;
                            }
                            kill(-pid, sig);
                        }
                    });
                }
            }
            Cmd::Interrupt { slot } => {
                let line = serde_json::json!({
                    "type": "control_request",
                    "request_id": format!("tend-{}", now_ms()),
                    "request": {"subtype": "interrupt"},
                });
                self.write_line(slot, &line);
            }
            Cmd::Deliver { to, from, kind, text } => {
                let mut text = text.clone();
                if let (tend_core::fleet::spec::MsgKind::Task, Address::Slot { id }) = (kind, to) {
                    // Its dependencies' work first, then the base the audit diffs from.
                    if let Some(note) = self.bring_in_dependencies(id, &text) {
                        text = format!("{text}\n\n{note}");
                    }
                    self.record_base(id, &text);
                }
                let text = &text;
                let from = match from {
                    Actor::User => "the user".to_string(),
                    Actor::Tend => "tend".to_string(),
                    Actor::Slot { id } => id.clone(),
                };
                let kind = format!("{kind:?}").to_lowercase();
                match to {
                    // A message (not a task) for a session that is busy, or in a terminal,
                    // waits in its inbox; its daemon shows it at the next hook (F4). In look
                    // mode a headless session gets it as a turn, since hooks show nothing.
                    Address::Slot { id }
                        if kind != "task"
                            && (self.procs.get(id).is_none_or(|p| p.stdin.is_none())
                                || (self.meta.mode == Mode::Optimize
                                    && self.model.slots().get(id).is_some_and(|s| s.busy))) =>
                    {
                        let line = serde_json::json!({"ts_ms": now_ms(), "from": from, "kind": kind, "text": text});
                        let path = self.dir.join("slots").join(format!("{}.inbox.jsonl", file_name(id)));
                        let _ = paths::ensure_private_dir(&self.dir.join("slots"));
                        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    Address::Slot { id } => {
                        let line = serde_json::json!({
                            "type": "user",
                            "message": {"role": "user", "content": format!("[tend fleet: {kind} from {from}]\n{text}")},
                        });
                        if !self.write_line(id, &line) {
                            inbox(&self.dir, &format!("Undelivered {kind} for {id} from {from}: {text}"));
                        }
                    }
                    _ => inbox(&self.dir, &format!("{kind} from {from}: {text}")),
                }
            }
            Cmd::Notify { text } => inbox(&self.dir, text),
            Cmd::Attach { role } => inbox(
                &self.dir,
                &format!("Role {role} runs in your terminal: start it in this repo with `tend claude --tend-fleet {} --tend-role {role}`.", self.meta.id),
            ),
            // Answers go back with the request's reply; waits need nothing done.
            Cmd::Release { slot } => {
                if let Some(p) = self.procs.get_mut(slot) {
                    p.stdin = None;
                }
            }
            Cmd::RunCheck { task, slot, command } => self.run_check(task, slot, command),
            Cmd::Backoff { .. } | Cmd::TaskRetry { .. } | Cmd::Answer { .. } => {}
        }
    }

    /// Queue one JSON line for a slot's stdin. False if it has none.
    fn write_line(&mut self, slot: &str, v: &serde_json::Value) -> bool {
        let Some(stdin) = self.procs.get(slot).and_then(|p| p.stdin.as_ref()) else {
            return false;
        };
        stdin.send(v.to_string()).is_ok()
    }

    fn spawn(
        &mut self,
        slot: &str,
        workspace: &Workspace,
        model: Option<&str>,
        permission_mode: Option<&str>,
        brief: &str,
    ) -> Result<String> {
        let cwd = self.workspace_dir(slot, workspace)?;
        // A stream slot that ran before resumes its conversation (G1); a oneshot always
        // starts fresh, as a task's attempts do.
        let resume = self
            .model
            .role_of(slot)
            .filter(|r| r.run == Run::Stream)
            .and(self.last_session.get(slot).cloned());
        let session = match &resume {
            Some(s) => s.clone(),
            None => uuid_v4()?,
        };
        let exe = std::env::current_exe().context("find tend executable")?;
        let files = self.dir.join("slots");
        paths::ensure_private_dir(&files)?;
        let name = file_name(slot);
        let err = OpenOptions::new()
            .create(true)
            .append(true)
            .open(files.join(format!("{name}.err")))?;
        let mode = match self.meta.mode {
            Mode::Look => "look",
            Mode::Optimize => "optimize",
        };
        let mut cmd = Command::new(exe);
        cmd.args(["claude", "--tend-mode", mode, "-p"])
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ])
            .args(match &resume {
                Some(s) => ["--resume", s.as_str()],
                None => ["--session-id", session.as_str()],
            })
            .arg("--append-system-prompt")
            .arg(brief);
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        if let Some(p) = permission_mode {
            cmd.args(["--permission-mode", p]);
        }
        let mut child = cmd
            .current_dir(&cwd)
            .env("TEND_FLEET", &self.meta.id)
            .env("TEND_FLEET_SLOT", slot)
            .env("TEND_FLEET_RULES", self.rules_for(slot, &cwd))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(err)
            .process_group(0)
            .spawn()
            .context("start tend claude")?;
        let pid = child.id() as i32;
        // For a restarted hub to stop it (G1): the launcher leads the session's group.
        let _ = fs::write(files.join(format!("{name}.pid")), pid.to_string());
        // Its own writer: a full pipe blocks this thread, not the hub.
        let stdin = child.stdin.take().map(|mut pipe| {
            let (tx, rx) = mpsc::channel::<String>();
            thread::spawn(move || {
                for mut line in rx {
                    line.push('\n');
                    if pipe
                        .write_all(line.as_bytes())
                        .and_then(|_| pipe.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            });
            tx
        });
        let stdout = child.stdout.take();
        let exited = Arc::new(AtomicBool::new(false));
        if let Some(out) = stdout {
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(files.join(format!("{name}.jsonl")))?;
            let (tx, slot) = (self.tx.clone(), slot.to_string());
            thread::spawn(move || read_output(out, log, &slot, &tx));
        }
        {
            let (tx, slot, exited) = (self.tx.clone(), slot.to_string(), Arc::clone(&exited));
            thread::spawn(move || {
                let code = child.wait().ok().and_then(|s| s.code());
                exited.store(true, Ordering::SeqCst);
                let _ = tx.send(Msg::Event(FleetEvent::Exited { slot, code }));
            });
        }
        self.worktrees.insert(slot.to_string(), cwd.clone());
        self.procs.insert(
            slot.to_string(),
            Proc {
                pid,
                stdin,
                turns: 0,
                exited,
                worktree: cwd,
            },
        );
        Ok(session)
    }

    /// The slot's working directory: its own worktree (made once, kept across restarts),
    /// a team's shared one, or the checkout itself.
    fn workspace_dir(&self, slot: &str, ws: &Workspace) -> Result<PathBuf> {
        let (Some(repo), Some(base)) = (&self.meta.repo, &self.meta.base) else {
            return Ok(PathBuf::from(&self.meta.cwd));
        };
        let name = match ws {
            Workspace::Shared => return Ok(PathBuf::from(&self.meta.cwd)),
            Workspace::WorktreePerWorker => slot.to_string(),
            Workspace::Team { name } => format!("team-{name}"),
        };
        let path = self.dir.join("worktrees").join(file_name(&name));
        if path.exists() {
            return Ok(path);
        }
        paths::ensure_private_dir(&self.dir.join("worktrees"))?;
        let branch = branch_name(&self.meta.id, &name);
        let repo = Path::new(repo);
        let have_branch = git_out(repo, &["rev-parse", "--verify", "--quiet", &branch]).is_some();
        let mut args = vec!["worktree", "add", "--quiet"];
        let p = path.to_string_lossy().to_string();
        if have_branch {
            args.extend([p.as_str(), branch.as_str()]);
        } else {
            args.extend(["-b", branch.as_str(), p.as_str(), base.as_str()]);
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .context("run git worktree add")?;
        if !out.status.success() {
            bail!(
                "git worktree add: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(path)
    }

    /// A fresh worker's worktree starts at the fleet's base, without the work of the tasks
    /// its task depends on: bring their changes in (tracked changes as a patch, new
    /// files copied), once, before its first task. The note says what came in, or what
    /// could not. A judge's parts are not merged: it compares them.
    fn bring_in_dependencies(&mut self, slot: &str, text: &str) -> Option<String> {
        let tid = text.strip_prefix("Task ")?.split(':').next()?.to_string();
        let task = self.model.tasks().find(|t| t.id == tid)?.clone();
        let fresh = self.model.slots().get(slot).is_some_and(|s| {
            !self.integrated.contains(slot) && s.task.as_deref() == Some(tid.as_str())
        });
        self.integrated.insert(slot.to_string());
        if !fresh || task.depends_on.is_empty() {
            return None;
        }
        if self.model.role_of(slot).map(|r| &r.workspace) != Some(&Workspace::WorktreePerWorker) {
            return None;
        }
        let target = self.worktrees.get(slot)?.clone();
        // Its dependencies and theirs, each once, the earliest first.
        let mut want: Vec<String> = Vec::new();
        let mut stack: Vec<String> = task.depends_on.clone();
        while let Some(d) = stack.pop() {
            if want.contains(&d) {
                continue;
            }
            if let Some(dt) = self.model.tasks().find(|t| t.id == d) {
                if dt.parent.as_deref() == Some(tid.as_str()) {
                    continue;
                }
                stack.extend(dt.depends_on.clone());
                want.push(d);
            }
        }
        let order: Vec<String> = self
            .model
            .tasks()
            .filter(|t| want.contains(&t.id))
            .map(|t| t.id.clone())
            .collect();
        let (mut brought, mut failed) = (Vec::new(), Vec::new());
        let mut conflicted: Vec<String> = Vec::new();
        for dep in order {
            match self.move_work(&dep, &target, false) {
                Ok(m) if m.conflicts.is_empty() && m.moved => brought.push(dep),
                Ok(m) if m.moved => {
                    conflicted.extend(m.conflicts.iter().cloned());
                    brought.push(dep);
                }
                Ok(_) => {}
                Err(e) => failed.push(format!("{dep}: {e}")),
            }
        }
        let mut note = Vec::new();
        if !brought.is_empty() {
            let who: Vec<String> = brought
                .iter()
                .map(|d| {
                    let slot = self
                        .model
                        .tasks()
                        .find(|t| t.id == *d)
                        .and_then(|t| t.slot.clone());
                    format!("{d} ({})", slot.unwrap_or_default())
                })
                .collect();
            note.push(format!(
                "tend brought the work of {} into this worktree before your task.",
                who.join(", ")
            ));
        }
        if !conflicted.is_empty() {
            conflicted.sort();
            conflicted.dedup();
            note.push(format!(
                "Their changes conflicted in {}: resolve the conflict markers there first.",
                conflicted.join(", ")
            ));
        }
        if !failed.is_empty() {
            note.push(format!("tend could not bring in: {}.", failed.join("; ")));
        }
        eprintln!("tend fleet hub: {slot} for {tid}: {}", note.join(" "));
        (!note.is_empty()).then(|| note.join("\n"))
    }

    /// Move task `tid`'s own work into the worktree `target`, by a 3-way merge of trees:
    /// the tree the task started at, its worktree now, and the target now. Commit
    /// objects only, no ref moves. `up` (into a lead's worktree): a conflict is an error,
    /// nothing is written; else the merge is written with conflict markers, and the
    /// files are named. Without `git merge-tree --write-tree` (git < 2.40), a patch.
    fn move_work(&self, tid: &str, target: &Path, up: bool) -> std::result::Result<Moved, String> {
        let task = self
            .model
            .tasks()
            .find(|t| t.id == tid)
            .ok_or("no such task")?;
        let src = task
            .slot
            .as_ref()
            .and_then(|s| self.worktrees.get(s).cloned().or_else(|| self.slot_dir(s)))
            .ok_or("its worktree is gone")?;
        // Whole worktrees: `git apply` in a subdirectory would skip what is outside it.
        let top = |p: &Path| crate::audit::repo_root(p).unwrap_or_else(|_| p.to_path_buf());
        let (src, target) = (top(&src), top(target));
        let target = target.as_path();
        if src == target {
            return Ok(Moved::default());
        }
        let scratch = self.dir.join("scratch");
        let _ = paths::ensure_private_dir(&scratch);
        let snap =
            |wt: &Path| crate::audit::worktree_tree(wt, &scratch).map_err(|e| format!("{e:#}"));
        let base = match self.bases.get(tid) {
            Some((_, tree)) => tree.clone(),
            None => {
                let c = self
                    .meta
                    .base
                    .clone()
                    .ok_or("the fleet has no base commit")?;
                git_out(&src, &["rev-parse", &format!("{c}^{{tree}}")]).ok_or("no base tree")?
            }
        };
        let (from, into) = (snap(&src)?, snap(target)?);
        if from == base {
            return Ok(Moved::default());
        }
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(target)
                .args(args)
                .env("GIT_AUTHOR_NAME", "tend")
                .env("GIT_AUTHOR_EMAIL", "tend@localhost")
                .env("GIT_COMMITTER_NAME", "tend")
                .env("GIT_COMMITTER_EMAIL", "tend@localhost")
                .stdin(Stdio::null())
                .output()
        };
        let commit = |tree: &str, parent: Option<&str>| -> std::result::Result<String, String> {
            let mut args = vec!["commit-tree", tree, "-m", "tend"];
            if let Some(p) = parent {
                args.extend(["-p", p]);
            }
            let o = git(&args).map_err(|e| e.to_string())?;
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .ok_or_else(|| String::from_utf8_lossy(&o.stderr).trim().to_string())
        };
        let b = commit(&base, None)?;
        let (theirs, ours) = (commit(&from, Some(&b))?, commit(&into, Some(&b))?);
        let mb = format!("--merge-base={b}");
        let merged = git(&[
            "merge-tree",
            "--write-tree",
            "--name-only",
            &mb,
            &ours,
            &theirs,
        ])
        .map_err(|e| e.to_string())?;
        let code = merged.status.code();
        if !matches!(code, Some(0) | Some(1)) {
            // No usable merge-tree: the patch, as before.
            return self
                .move_work_by_patch(&src, &base, target)
                .map(|moved| Moved {
                    moved,
                    conflicts: vec![],
                });
        }
        let text = String::from_utf8_lossy(&merged.stdout).to_string();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or("").trim().to_string();
        let conflicts: Vec<String> = if code == Some(1) {
            let mut c: Vec<String> = lines
                .take_while(|l| !l.is_empty())
                .map(String::from)
                .collect();
            c.sort();
            c.dedup();
            c
        } else {
            vec![]
        };
        if up && !conflicts.is_empty() {
            return Err(format!(
                "it conflicts with your changes in {} (its work is in {})",
                conflicts.join(", "),
                src.display()
            ));
        }
        if tree == into {
            return Ok(Moved::default());
        }
        let diff =
            git(&["diff-tree", "-p", "--binary", &into, &tree]).map_err(|e| e.to_string())?;
        let mut apply = Command::new("git")
            .arg("-C")
            .arg(target)
            .args(["apply", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut i) = apply.stdin.take() {
            let _ = i.write_all(&diff.stdout);
        }
        let out = apply.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("git apply failed")
                .to_string());
        }
        Ok(Moved {
            moved: true,
            conflicts,
        })
    }

    /// The patch way (older git): the diff from `base`, and new files copied.
    fn move_work_by_patch(
        &self,
        src: &Path,
        base: &str,
        target: &Path,
    ) -> std::result::Result<bool, String> {
        let diff = Command::new("git")
            .arg("-C")
            .arg(src)
            .args([
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                base,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        let mut moved = false;
        if !diff.stdout.is_empty() {
            let mut apply = Command::new("git")
                .arg("-C")
                .arg(target)
                .args(["apply", "--whitespace=nowarn", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            if let Some(mut i) = apply.stdin.take() {
                let _ = i.write_all(&diff.stdout);
            }
            let out = apply.wait_with_output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("git apply failed")
                    .to_string());
            }
            moved = true;
        }
        let new = git_out(
            src,
            &[
                "-c",
                "core.quotePath=false",
                "ls-files",
                "--others",
                "--exclude-standard",
            ],
        )
        .unwrap_or_default();
        for f in new.lines() {
            let to = target.join(f);
            if to.exists() {
                continue;
            }
            if let Some(p) = to.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            fs::copy(src.join(f), &to).map_err(|e| format!("copy {f}: {e}"))?;
            moved = true;
        }
        Ok(moved)
    }

    /// A part that is done brings its work up, into the worktree of the session that
    /// holds its parent task (a lead that split its task). A best-of's candidates are
    /// left apart: the judge picks one.
    fn bring_up_done_parts(&mut self) {
        use tend_core::fleet::model::TaskState;
        let done: Vec<(String, String)> = self
            .model
            .tasks()
            .filter(|t| t.state == TaskState::Done && !self.lifted.contains(&t.id))
            .filter_map(|t| Some((t.id.clone(), t.parent.clone()?)))
            .collect();
        for (tid, parent) in done {
            self.lifted.insert(tid.clone());
            let Some(p) = self.model.tasks().find(|t| t.id == parent).cloned() else {
                continue;
            };
            if p.best_of || p.target.as_deref() == Some("judge") {
                continue;
            }
            let Some(target) = p.slot.as_ref().and_then(|s| self.worktrees.get(s)).cloned() else {
                continue;
            };
            let holder = p.slot.clone().unwrap_or_default();
            match self.move_work(&tid, &target, true) {
                Ok(m) if m.moved => {
                    eprintln!("tend fleet hub: {tid}'s work is in {holder}'s worktree now")
                }
                Ok(_) => {}
                Err(e) => {
                    // The lead hears it: its whole check cannot pass without this part.
                    let text = format!(
                        "tend could not bring {tid}'s work into your worktree: {e}. Merge it by hand, then go on."
                    );
                    let line = serde_json::json!({
                        "type": "user",
                        "message": {"role": "user", "content": format!("[tend fleet: notice from tend]\n{text}")},
                    });
                    self.write_line(&holder, &line);
                    inbox(&self.dir, &format!("{holder}: {text}"));
                }
            }
        }
    }

    /// With the audit on: the slot's worktree tree when it gets a task, to diff at check
    /// time. The task id is the delivered text's first words (`Task t-2: …`).
    fn record_base(&mut self, slot: &str, text: &str) {
        let Some(task) = text
            .strip_prefix("Task ")
            .and_then(|t| t.split(':').next())
            .map(String::from)
        else {
            return;
        };
        let Some(wt) = self.worktrees.get(slot) else {
            return;
        };
        let Ok(root) = crate::audit::repo_root(wt) else {
            return;
        };
        let scratch = self.dir.join("scratch");
        let _ = paths::ensure_private_dir(&scratch);
        // Once per task: a retry to the same session (or a lead's message that looks like
        // a task) must not move the start past the first attempt's work.
        let holds =
            self.model.slots().get(slot).and_then(|s| s.task.as_deref()) == Some(task.as_str());
        if !holds || self.bases.contains_key(&task) {
            return;
        }
        if let Ok(tree) = crate::audit::worktree_tree(&root, &scratch) {
            self.bases.insert(task, (root, tree));
        }
    }

    /// Run a task's check in its slot's worktree, off the hub's loop; the result comes
    /// back as a CheckResult event.
    fn run_check(&mut self, task: &str, slot: &str, command: &str) {
        let tx = self.tx.clone();
        let (task, command) = (task.to_string(), command.to_string());
        let wt = self
            .worktrees
            .get(slot)
            .cloned()
            .or_else(|| self.slot_dir(slot));
        let base = self.bases.get(&task).cloned();
        let scratch = self.dir.join("scratch");
        let lock = Arc::clone(&self.checking);
        let audit = self.audit;
        thread::spawn(move || {
            let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
            let event = match wt {
                Some(wt) => {
                    let (exit, output) = run_with_timeout(&wt, &command, CHECK_TIMEOUT);
                    let passed = exit == Some(0);
                    let flags = match (&base, passed && audit) {
                        (Some((root, from)), true) => audit_flags(root, from, &scratch),
                        _ => Vec::new(),
                    };
                    let diff_lines = if passed {
                        diff_size(&wt, base.as_ref().map(|b| b.1.as_str()))
                    } else {
                        None
                    };
                    FleetEvent::CheckResult {
                        task,
                        passed,
                        exit,
                        excerpt: if passed {
                            String::new()
                        } else {
                            excerpt(&output)
                        },
                        flags,
                        diff_lines,
                    }
                }
                None => FleetEvent::CheckResult {
                    task,
                    passed: false,
                    exit: None,
                    excerpt: "tend found no worktree for the session to check.".into(),
                    flags: Vec::new(),
                    diff_lines: None,
                },
            };
            let _ = tx.send(Msg::Event(event));
        });
    }

    /// A terminal session joins as an attached role (F4). The reply is JSON with its slot
    /// and rules, or a refusal.
    fn join(
        &mut self,
        role: Option<String>,
        name: Option<String>,
        pid: i32,
        cwd: String,
    ) -> String {
        let attached: Vec<&String> = self
            .meta
            .spec
            .roles
            .iter()
            .filter(|(_, r)| r.run == Run::Attached)
            .map(|(n, _)| n)
            .collect();
        let role = match (role, attached.as_slice()) {
            (Some(r), _) => r,
            (None, [one]) => one.to_string(),
            (None, []) => return "tend: refused. This fleet has no attached role; add one with run = \"attached\".".into(),
            (None, _) => return "tend: refused. This fleet has several attached roles; pick one with --tend-role.".into(),
        };
        let recs = self.apply(FleetEvent::Attached {
            role,
            name,
            session: format!("pid-{pid}"),
        });
        let answer = recs.iter().find_map(|r| match &r.body {
            FleetBody::Command {
                command: Cmd::Answer { text, .. },
                ..
            } => Some(text.clone()),
            _ => None,
        });
        let Some(slot) = answer.clone().filter(|a| !a.starts_with("tend: refused")) else {
            return answer.unwrap_or_else(|| "tend: refused.".into());
        };
        let cwd = PathBuf::from(cwd);
        self.worktrees.insert(slot.clone(), cwd.clone());
        self.attached.insert(slot.clone());
        self.procs.insert(
            slot.clone(),
            Proc {
                pid,
                stdin: None,
                turns: 0,
                exited: Arc::new(AtomicBool::new(false)),
                worktree: cwd.clone(),
            },
        );
        serde_json::json!({"slot": slot, "rules": self.rules_for(&slot, &cwd)}).to_string()
    }

    /// A session's messages (its inbox file), or the fleet's inbox for the user.
    fn inbox_of(&self, by: &Actor) -> String {
        let path = match by {
            Actor::Slot { id } => self
                .dir
                .join("slots")
                .join(format!("{}.inbox.jsonl", file_name(id))),
            _ => self.dir.join("inbox.jsonl"),
        };
        render_inbox(&path)
    }

    /// Where a slot works, without making anything: its worktree if it exists, or the
    /// checkout (G1: a restarted hub, a check after its session ended).
    fn slot_dir(&self, slot: &str) -> Option<PathBuf> {
        let name = match &self.model.role_of(slot)?.workspace {
            Workspace::Shared => return Some(PathBuf::from(&self.meta.cwd)),
            Workspace::WorktreePerWorker => slot.to_string(),
            Workspace::Team { name } => format!("team-{name}"),
        };
        let p = self.dir.join("worktrees").join(file_name(&name));
        if p.exists() {
            Some(p)
        } else if self.meta.base.is_none() {
            Some(PathBuf::from(&self.meta.cwd))
        } else {
            None
        }
    }

    /// A session's write rules (F5c), as JSON for `TEND_FLEET_RULES`.
    fn rules_for(&self, slot: &str, cwd: &Path) -> String {
        let role = self.model.role_of(slot);
        let rules = tend_core::fleet::scope::RulesFile {
            root: cwd.display().to_string(),
            slot: slot.to_string(),
            may_write: role.map(|r| r.may_write.clone()).unwrap_or_default(),
            may_not_write: role.map(|r| r.may_not_write.clone()).unwrap_or_default(),
            claims: Some(self.dir.join("claims.json").display().to_string()),
            inbox: Some(
                self.dir
                    .join("slots")
                    .join(format!("{}.inbox.jsonl", file_name(slot)))
                    .display()
                    .to_string(),
            ),
        };
        serde_json::to_string(&rules).unwrap_or_default()
    }

    /// The scopes of tasks being worked on, for sessions in the same worktree (F5c).
    /// Written only when they change, by rename, so a reader never sees half a file.
    fn write_claims(&mut self) {
        use tend_core::fleet::model::TaskState;
        let claims: Vec<tend_core::fleet::scope::ClaimFile> = self
            .model
            .tasks()
            .filter(|t| {
                matches!(
                    t.state,
                    TaskState::Working | TaskState::Checking | TaskState::Bound
                )
            })
            .filter(|t| !t.scope.is_empty())
            .filter_map(|t| {
                let slot = t.slot.clone()?;
                let worktree = self.worktrees.get(&slot)?.display().to_string();
                Some(tend_core::fleet::scope::ClaimFile {
                    slot,
                    task: t.id.clone(),
                    worktree,
                    scope: t.scope.clone(),
                })
            })
            .collect();
        let text = serde_json::to_string(&claims).unwrap_or_default();
        if self.claims_text.as_deref() == Some(text.as_str()) {
            return;
        }
        let tmp = self.dir.join("claims.json.t");
        if fs::write(&tmp, &text).is_ok() && fs::rename(&tmp, self.dir.join("claims.json")).is_ok()
        {
            self.claims_text = Some(text);
        }
    }

    fn status(&self) -> String {
        let mut out = render_status(&self.meta, &self.model, &self.procs, true);
        out.push_str(&render_tasks(&self.model));
        out
    }
}

/// A check that runs longer is stopped and fails.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Output kept from a check, at most.
const CHECK_OUTPUT_BYTES: usize = 4 << 20;

/// Run `command` with `sh` in `dir`, stdout and stderr together. Its exit code (None:
/// killed or timed out) and output.
fn run_with_timeout(dir: &Path, command: &str, timeout: Duration) -> (Option<i32>, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (None, format!("could not run the check: {e}")),
    };
    let out = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = out {
            let _ = (&mut out)
                .take(CHECK_OUTPUT_BYTES as u64)
                .read_to_end(&mut buf);
            // Past the cap: keep reading, so the check is not blocked or killed by SIGPIPE.
            let _ = std::io::copy(&mut out, &mut std::io::sink());
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let start = std::time::Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Anything the check left running holds the pipe open and would block
                // the reader, and every later check behind the lock.
                kill(-(child.id() as i32), libc::SIGKILL);
                break status.code();
            }
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(100)),
            _ => {
                kill(-(child.id() as i32), libc::SIGKILL);
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut output = reader.join().unwrap_or_default();
    if code.is_none() && start.elapsed() >= timeout {
        output.push_str(&format!(
            "\n[tend: the check ran over {} min and was stopped]",
            timeout.as_secs() / 60
        ));
    }
    (code, output)
}

/// What a session needs from a failed check: the failures tend's parsers find, or the
/// end of the output.
fn excerpt(output: &str) -> String {
    let failures = tend_core::perception::extract_failures(output);
    if !failures.is_empty() {
        return failures
            .iter()
            .take(10)
            .map(|f| match &f.test {
                Some(t) => format!("{t}: {}", f.message),
                None => f.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let lines: Vec<&str> = output.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// Lines a worktree changed (added plus removed): from the task's base tree when the
/// audit recorded one, else from the worktree's HEAD; new files count whole (G9).
fn diff_size(wt: &Path, base_tree: Option<&str>) -> Option<u64> {
    let from = base_tree.unwrap_or("HEAD");
    let numstat = git_out(wt, &["diff", "--numstat", from]).unwrap_or_default();
    let mut n: u64 = numstat
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some(f.next()?.parse::<u64>().unwrap_or(0) + f.next()?.parse::<u64>().unwrap_or(0))
        })
        .sum();
    let new = git_out(
        wt,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
    )
    .unwrap_or_default();
    for f in new.lines() {
        n += fs::read_to_string(wt.join(f)).map_or(0, |t| t.lines().count() as u64);
    }
    Some(n)
}

/// The audit of a task's diff (experimental): what may weaken tests or checks.
fn audit_flags(root: &Path, from: &str, scratch: &Path) -> Vec<String> {
    let Ok(to) = crate::audit::worktree_tree(root, scratch) else {
        return Vec::new();
    };
    let policy = crate::config::load_policy(root).0;
    let Ok(report) = crate::audit::run(root, from, &to, &|p| policy.audit_ignores(p)) else {
        return Vec::new();
    };
    let mut flags: Vec<String> = report
        .tests
        .iter()
        .filter(|t| !t.weakened.is_empty())
        .map(|t| format!("{}: {}", t.path, t.weakened.join(", ")))
        .collect();
    flags.extend(
        report
            .config
            .iter()
            .filter(|c| !c.weaker.is_empty())
            .map(|c| format!("{}: {}", c.path, c.weaker.join(", "))),
    );
    flags
}

/// Copy a slot's output to its log, and tell the hub about activity and turn ends.
fn read_output(out: impl Read, mut log: File, slot: &str, tx: &Sender<Msg>) {
    let mut last = 0;
    let mut reader = BufReader::new(out);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        // Bytes, not `lines()`: a line that is not UTF-8 must not end the reading.
        if !matches!(reader.read_until(b'\n', &mut raw), Ok(n) if n > 0) {
            break;
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        let _ = writeln!(log, "{line}");
        let kind = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from));
        let sent = if kind.as_deref() == Some("result") {
            if let Some(e) = usage_of(line, slot) {
                let _ = tx.send(Msg::Event(e));
            }
            if let Some(e) = limit_of(line, slot) {
                let _ = tx.send(Msg::Event(e));
            }
            tx.send(Msg::TurnEnded(slot.to_string()))
        } else if now_ms().saturating_sub(last) >= ACTIVITY_EVERY_MS {
            last = now_ms();
            tx.send(Msg::Event(FleetEvent::Activity {
                slot: slot.to_string(),
                turn_ended: false,
            }))
        } else {
            Ok(())
        };
        if sent.is_err() {
            return;
        }
    }
}

/// A restarted hub stops the sessions the old one left: each slot's process group,
/// from `slots/<slot>.pid`, is sent SIGTERM, then SIGKILL after a second.
fn stop_old_sessions(dir: &Path) {
    let mut groups = Vec::new();
    for e in fs::read_dir(dir.join("slots"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "pid") {
            if let Some(pid) = fs::read_to_string(&p)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                // SAFETY: signal 0 only checks that the group exists. The pid may have
                // been reused since: signal it only if it is still a `tend claude`.
                if pid > 1 && unsafe { libc::kill(-pid, 0) } == 0 && is_tend_claude(pid) {
                    kill(-pid, libc::SIGTERM);
                    groups.push(pid);
                }
            }
            let _ = fs::remove_file(&p);
        }
    }
    if !groups.is_empty() {
        thread::sleep(Duration::from_secs(1));
        for pid in groups {
            kill(-pid, libc::SIGKILL);
        }
    }
}

fn is_tend_claude(pid: i32) -> bool {
    let cmd = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .or_else(|_| {
            Command::new("ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        })
        .unwrap_or_default();
    cmd.contains("tend") && cmd.contains("claude") && cmd.contains("stream-json")
}

/// The pid of the process at the other end of a Unix socket.
#[cfg(target_os = "linux")]
fn peer_pid(s: &UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a ucred.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid)
}

#[cfg(target_os = "macos")]
fn peer_pid(s: &UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    // <sys/un.h>: SOL_LOCAL 0, LOCAL_PEERPID 0x002.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pid`, a pid_t.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_pid(_: &UnixStream) -> Option<i32> {
    None
}

/// `pid` and its parents, up to init. A process that left its tree (a double fork)
/// is not found: the agent-side check (`fleet_bypass`) covers the plain cases.
fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = vec![pid];
    let mut at = pid;
    for _ in 0..64 {
        let Some(parent) = parent_of(at).filter(|&p| p > 1) else {
            break;
        };
        out.push(parent);
        at = parent;
    }
    out
}

fn parent_of(pid: i32) -> Option<i32> {
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        // pid (comm) state ppid …; comm may hold spaces and parens.
        return stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok();
    }
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A result line's usage: the turn's tokens (prompt, cache, and output) and the
/// session's cost so far (F5d).
fn usage_of(line: &str, slot: &str) -> Option<FleetEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let u = v.get("usage")?;
    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let tokens = n("input_tokens")
        + n("cache_creation_input_tokens")
        + n("cache_read_input_tokens")
        + n("output_tokens");
    Some(FleetEvent::Usage {
        slot: slot.to_string(),
        tokens,
        cost_usd: v
            .get("total_cost_usd")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0),
    })
}

/// A result that says the plan's usage limit was hit (the bench runner's signal).
fn limit_of(line: &str, slot: &str) -> Option<FleetEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
    let lower = text.to_lowercase();
    let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    let named = lower.contains("usage limit") || lower.contains("session limit");
    // Not an error: only a short line that says the limit was hit ("…usage limit
    // reached", "You've hit your session limit"), not an answer that mentions one.
    let reached = lower.contains("limit reached") || lower.contains("hit your");
    let hit = named && (is_error || (lower.len() < 300 && reached));
    hit.then(|| FleetEvent::UsageLimit {
        slot: slot.to_string(),
        text: text.to_string(),
    })
}

fn kill(pid: i32, sig: libc::c_int) {
    // SAFETY: kill has no memory effects; a stale pid is the only risk, and the
    // caller checks `exited` first.
    unsafe {
        libc::kill(pid, sig);
    }
}

/// Slot ids hold `/` in teams; a file or directory name may not.
fn file_name(slot: &str) -> String {
    slot.replace('/', "__")
}

/// A slot's branch. Nested slots are spelled flat (`sub-1__worker-1`): git cannot have
/// `tend/F/sub-1` and `tend/F/sub-1/worker-1` at once.
fn branch_name(fleet: &str, name: &str) -> String {
    format!("tend/{fleet}/{}", file_name(name))
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A random version-4 UUID, for `claude --session-id`.
fn uuid_v4() -> Result<String> {
    let mut b = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

fn inbox(dir: &Path, text: &str) {
    let line = serde_json::json!({"ts_ms": now_ms(), "text": text});
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("inbox.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

// --- The client side ---------------------------------------------------------------

/// How long a client waits for the hub. It serves one request at a time and moves
/// work with git; on a loaded machine (four workers running `make check`) 10 s was
/// not enough, and a test read "busy" as a failure (dogfood fleet, 2026-10-22).
const REPLY_WAIT: Duration = Duration::from_secs(30);

/// Send a request to a fleet's hub; its reply message.
pub fn send(dir: &Path, req: &HubRequest) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path(dir))?;
    stream.set_read_timeout(Some(REPLY_WAIT))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut out = String::new();
    if let Err(e) = BufReader::new(stream).read_line(&mut out) {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            // Not "down": the hub got it and is busy. Repeating it could do it twice.
            return Ok("tend: the hub is busy and did not answer in 30 s; your request may still apply. Check with `tend task list` or `tend fleet status` before you repeat it.".into());
        }
        return Err(e.into());
    }
    let r: Reply = serde_json::from_str(&out)?;
    Ok(r.message.unwrap_or_default())
}

fn is_up(dir: &Path) -> bool {
    send(dir, &HubRequest::Ping).is_ok()
}

fn fleet_dirs() -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = fs::read_dir(fleets_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").exists())
        .collect();
    d.sort();
    d
}

/// The fleet dir named by `s`: its id, or a unique prefix of one.
fn pick(dirs: &[PathBuf], s: &str) -> Result<PathBuf> {
    let hits: Vec<&PathBuf> = dirs
        .iter()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(s))
        })
        .collect();
    // An id that is also the start of a longer one names itself.
    if let Some(exact) = hits.iter().find(|d| d.file_name().is_some_and(|n| n == s)) {
        return Ok((*exact).clone());
    }
    match hits[..] {
        [one] => Ok(one.clone()),
        [] => bail!("no fleet `{s}` (`tend fleet status` lists them)"),
        _ => bail!("`{s}` matches {} fleets; give more of the id", hits.len()),
    }
}

/// A fleet by id or unique prefix; else the newest one that is up; else the newest.
pub fn resolve(spec: Option<&str>) -> Result<PathBuf> {
    let dirs = fleet_dirs();
    if let Some(s) = spec {
        return pick(&dirs, s);
    }
    if let Some(d) = dirs.iter().rev().find(|d| is_up(d)) {
        return Ok(d.clone());
    }
    dirs.last()
        .cloned()
        .ok_or_else(|| anyhow!("no fleets yet; start one with `tend fleet up`"))
}

fn read_meta(dir: &Path) -> Result<Meta> {
    Ok(serde_json::from_str(&fs::read_to_string(
        dir.join("meta.json"),
    )?)?)
}

/// The fleet's records, from its ledger.
pub fn read_ledger(dir: &Path) -> Vec<FleetRecord> {
    let Ok(text) = fs::read_to_string(dir.join("ledger.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The model a fleet's ledger describes: its events, replayed.
fn replay(meta: &Meta, records: &[FleetRecord]) -> Model {
    let mut m = Model::new(meta.spec.clone());
    for r in records {
        if let FleetBody::Event { event } = &r.body {
            m.step(event.clone(), r.ts_ms);
        }
    }
    m
}

/// `tend fleet status`: the hub's view when it is up, else the ledger's.
pub fn status(spec: Option<&str>) -> Result<String> {
    let dir = resolve(spec)?;
    if let Ok(text) = send(&dir, &HubRequest::Status) {
        return Ok(text);
    }
    let meta = read_meta(&dir)?;
    let model = replay(&meta, &read_ledger(&dir));
    Ok(render_status(&meta, &model, &BTreeMap::new(), false))
}

fn render_status(meta: &Meta, model: &Model, procs: &BTreeMap<String, Proc>, up: bool) -> String {
    let mut out = String::new();
    let mode = match meta.mode {
        Mode::Look => "look",
        Mode::Optimize => "optimize",
    };
    let preset = serde_json::to_value(meta.spec.preset)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "fleet {} · {} · {preset} · {mode} mode · {}",
        meta.id,
        if up { "up" } else { "hub not running" },
        meta.repo.as_deref().unwrap_or(&meta.cwd)
    );
    let width = model.slots().keys().map(|k| k.len()).max().unwrap_or(4);
    for (id, s) in model.slots() {
        let state = format!("{:?}", s.state).to_lowercase();
        let mut facts = Vec::new();
        if let Some(p) = procs.get(id) {
            facts.push(format!("pid {}", p.pid));
            facts.push(format!(
                "{} turn{}",
                p.turns,
                if p.turns == 1 { "" } else { "s" }
            ));
            facts.push(p.worktree.display().to_string());
        }
        if s.tokens > 0 {
            facts.push(format!("{} tokens", crate::report::k(s.tokens)));
        }
        if s.restarts > 0 {
            facts.push(format!("restarts {}", s.restarts));
        }
        if s.state == SlotState::Waiting {
            if let Some(at) = s.retry_at {
                facts.push(format!(
                    "restart in {} s",
                    at.saturating_sub(now_ms()) / 1000
                ));
            }
        }
        let _ = writeln!(out, "  {id:<width$}  {state:<8}  {}", facts.join(" · "));
    }
    if model.slots().is_empty() {
        let _ = writeln!(out, "  no sessions");
    }
    let (tokens, cost) = model.spend();
    let _ = writeln!(
        out,
        "Spent: {} tokens · {}{}",
        crate::report::k(tokens),
        dollars(cost),
        if model.paused() {
            " · paused (`tend fleet resume`)"
        } else {
            ""
        }
    );
    out
}

/// A cost in dollars; under a cent is not shown as $0.00.
fn dollars(cost: f64) -> String {
    if cost > 0.0 && cost < 0.01 {
        "<$0.01".into()
    } else {
        format!("${cost:.2}")
    }
}

/// The task board, in the order tasks were added.
fn render_tasks(model: &Model) -> String {
    let mut out = String::new();
    let tasks: Vec<_> = model.tasks().collect();
    if tasks.is_empty() {
        return out;
    }
    let _ = writeln!(out, "Tasks:");
    let width = tasks.iter().map(|t| t.id.len()).max().unwrap_or(3);
    for t in tasks {
        let state = format!("{:?}", t.state).to_lowercase();
        let mut facts = vec![t.title.clone()];
        if let Some(s) = &t.slot {
            facts.push(format!("on {s}"));
        } else if let Some(x) = &t.target {
            facts.push(format!("for {x}"));
        }
        if !t.depends_on.is_empty() {
            facts.push(format!("after {}", t.depends_on.join(", ")));
        }
        if t.attempts > 0 {
            facts.push(format!(
                "{} failed attempt{}",
                t.attempts,
                if t.attempts == 1 { "" } else { "s" }
            ));
        }
        if let Some(o) = &t.outcome {
            facts.push(format!("\"{}\"", tend_core::text::truncate_chars(o, 80)));
        }
        let _ = writeln!(out, "  {:<width$}  {state:<8}  {}", t.id, facts.join(" · "));
    }
    out
}

/// The last 20 lines of an inbox file, oldest first.
fn render_inbox(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return "No messages.".into();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for l in &lines[lines.len().saturating_sub(20)..] {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
            continue;
        };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let head = match (s("kind"), s("from")) {
            (k, f) if !f.is_empty() => format!("{k} from {f}: "),
            _ => String::new(),
        };
        let _ = writeln!(out, "- {head}{}", s("text"));
    }
    if out.is_empty() {
        out.push_str("No messages.");
    }
    out
}

/// `tend msg` and `tend inbox`, like `tend task`.
pub fn msg(fleet: Option<&str>, to: String, kind: Option<String>, text: String) -> Result<String> {
    let env_fleet = std::env::var("TEND_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    let req = HubRequest::Msg {
        to,
        kind,
        text,
        claimed_slot: std::env::var("TEND_FLEET_SLOT").ok(),
    };
    send(&dir, &req).map_err(|_| anyhow!("the fleet's hub is not running"))
}

pub fn show_inbox(fleet: Option<&str>) -> Result<String> {
    let env_fleet = std::env::var("TEND_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    // The user reads the fleet's inbox: mark it read (G6).
    if std::env::var_os("TEND_FLEET_SLOT").is_none() {
        let _ = fs::write(
            dir.join("inbox.read"),
            line_count(&dir.join("inbox.jsonl")).to_string(),
        );
    }
    let req = HubRequest::Inbox {
        claimed_slot: std::env::var("TEND_FLEET_SLOT").ok(),
    };
    match send(&dir, &req) {
        Ok(t) => Ok(t),
        // The hub is down: the user can still read the fleet's inbox.
        Err(_) => Ok(render_inbox(&dir.join("inbox.jsonl"))),
    }
}

/// Join a fleet from a terminal (`tend claude --tend-fleet`): the slot and its rules.
pub fn join(
    fleet: Option<&str>,
    role: Option<String>,
    name: Option<String>,
    pid: i32,
) -> Result<(String, String, String)> {
    let dir = resolve(fleet)?;
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cwd = std::env::current_dir()?.display().to_string();
    let reply = send(
        &dir,
        &HubRequest::Join {
            role,
            name,
            pid,
            cwd,
        },
    )
    .map_err(|_| anyhow!("the fleet's hub is not running"))?;
    let v: serde_json::Value = serde_json::from_str(&reply)
        .map_err(|_| anyhow!("{}", reply.trim_start_matches("tend: ")))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok((id, s("slot"), s("rules")))
}

pub fn leave(fleet: &str, slot: &str) {
    if let Ok(dir) = resolve(Some(fleet)) {
        let _ = send(&dir, &HubRequest::Leave { slot: slot.into() });
    }
}

/// `tend task …`: the fleet from `TEND_FLEET` (a fleet session), `--fleet`, or the
/// newest that is up.
pub fn task(fleet: Option<&str>, verb: TaskVerb) -> Result<String> {
    let env_fleet = std::env::var("TEND_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    let req = HubRequest::Task {
        verb,
        claimed_slot: std::env::var("TEND_FLEET_SLOT").ok(),
    };
    send(&dir, &req).map_err(|_| anyhow!("the fleet's hub is not running"))
}

/// `tend fleet` (no subcommand) lists fleets too; one line each.
pub fn list() -> String {
    let mut out = String::new();
    for d in fleet_dirs() {
        let Ok(meta) = read_meta(&d) else { continue };
        let records = read_ledger(&d);
        let model = replay(&meta, &records);
        let live = model
            .slots()
            .values()
            .filter(|s| matches!(s.state, SlotState::Starting | SlotState::Running))
            .count();
        let unread = unread_for_user(&d);
        let _ = writeln!(
            out,
            "fleet {} · {} · {} sessions running{} · {}",
            meta.id,
            if is_up(&d) { "up" } else { "down" },
            live,
            if unread > 0 {
                format!(" · {unread} unread (`tend inbox`)")
            } else {
                String::new()
            },
            meta.repo.as_deref().unwrap_or(&meta.cwd)
        );
    }
    out
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |t| t.lines().count())
}

/// The user's fleet messages since `tend inbox` last showed them (G6).
fn unread_for_user(dir: &Path) -> usize {
    let read: usize = fs::read_to_string(dir.join("inbox.read"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    line_count(&dir.join("inbox.jsonl")).saturating_sub(read)
}

/// Messages a fleet session's inbox holds that its agent has not seen yet: the lines
/// beyond the messages its ledger recorded (G6). For `tend status`.
pub fn unread_for_session(inbox: &Path, entries: &[tend_core::Entry]) -> usize {
    let seen = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                tend_core::Body::Event {
                    event: tend_core::Event::Message { .. }
                }
            )
        })
        .count();
    line_count(inbox).saturating_sub(seen)
}

/// `tend fleet prune`: worktrees of fleets that are down, with no uncommitted changes
/// and no commits beyond the fleet's base. Lists them; removes them with `yes`.
pub fn prune(yes: bool) -> Result<String> {
    let mut out = String::new();
    let (mut removed, mut kept) = (0, 0);
    for d in fleet_dirs() {
        if is_up(&d) {
            continue;
        }
        let Ok(meta) = read_meta(&d) else { continue };
        let (Some(repo), Some(base)) = (&meta.repo, &meta.base) else {
            continue;
        };
        let repo = Path::new(repo);
        let mut wts: Vec<PathBuf> = fs::read_dir(d.join("worktrees"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        wts.sort();
        for wt in wts {
            let name = wt
                .file_name()
                .map(|n| n.to_string_lossy().replace("__", "/"))
                .unwrap_or_default();
            let branch = branch_name(&meta.id, &name);
            let dirty = git_out(&wt, &["status", "--porcelain"]).is_some();
            let count = |range: String, at: &Path| {
                git_out(at, &["rev-list", "--count", &range])
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            // Its branch, or a detached HEAD in it, past the base: unmerged commits.
            let ahead =
                count(format!("{base}..{branch}"), repo).max(count(format!("{base}..HEAD"), &wt));
            let why = if dirty {
                Some("it has uncommitted changes".to_string())
            } else if ahead > 0 {
                Some(format!("{branch} has {ahead} commit(s) not in the base"))
            } else {
                None
            };
            match why {
                Some(w) => {
                    kept += 1;
                    let _ = writeln!(out, "keep   {} ({w})", wt.display());
                }
                None if yes => {
                    let ok = Command::new("git")
                        .arg("-C")
                        .arg(repo)
                        .args(["worktree", "remove"])
                        .arg(&wt)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    if ok {
                        let _ = git_out(repo, &["branch", "-D", &branch]);
                        removed += 1;
                        let _ = writeln!(out, "removed {}", wt.display());
                    } else {
                        kept += 1;
                        let _ =
                            writeln!(out, "keep   {} (git worktree remove failed)", wt.display());
                    }
                }
                None => {
                    removed += 1;
                    let _ = writeln!(out, "would remove {}", wt.display());
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "{} {removed} worktree{}, kept {kept}.{}",
        if yes { "Removed" } else { "Would remove" },
        if removed == 1 { "" } else { "s" },
        if yes || removed == 0 {
            ""
        } else {
            " Run `tend fleet prune --yes` to remove them."
        }
    );
    Ok(out)
}

/// `tend fleet report`: what a fleet did, from its ledger: tasks, sessions, spend, and
/// the merge plan.
pub fn report(spec: Option<&str>) -> Result<String> {
    use tend_core::fleet::model::TaskState;
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    let records = read_ledger(&dir);
    let model = replay(&meta, &records);
    let mut out = String::new();
    let span = match (records.first(), records.last()) {
        (Some(a), Some(b)) => crate::report::minutes(b.ts_ms.saturating_sub(a.ts_ms)),
        _ => "0 s".into(),
    };
    let _ = writeln!(
        out,
        "fleet {} · {} · {} · ran {span}",
        meta.id,
        serde_json::to_value(meta.spec.preset)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
        if is_up(&dir) { "up" } else { "down" },
    );
    let tasks: Vec<_> = model.tasks().collect();
    let count = |st: TaskState| tasks.iter().filter(|t| t.state == st).count();
    let lost: u32 = tasks.iter().map(|t| t.attempts).sum();
    let _ = writeln!(
        out,
        "Tasks: {} · done {} · failed {} · blocked {} · in review {} · other {} · attempts that did not count: {lost}",
        tasks.len(),
        count(TaskState::Done),
        count(TaskState::Failed),
        count(TaskState::Blocked),
        count(TaskState::Review),
        tasks.len()
            - count(TaskState::Done)
            - count(TaskState::Failed)
            - count(TaskState::Blocked)
            - count(TaskState::Review),
    );
    out.push_str(&render_tasks(&model).replacen("Tasks:\n", "", 1));
    let restarts: u32 = model.slots().values().map(|s| s.restarts).sum();
    let (tokens, cost) = model.spend();
    let _ = writeln!(
        out,
        "Sessions: {} · restarts {restarts} · {} tokens · {}",
        model.slots().len(),
        crate::report::k(tokens),
        dollars(cost)
    );
    for (id, s) in model.slots() {
        let _ = writeln!(
            out,
            "  {id:<14} {:<8} {} tokens",
            format!("{:?}", s.state).to_lowercase(),
            crate::report::k(s.tokens)
        );
    }
    out.push_str(&merge_plan(&dir, &meta, &model, false)?);
    Ok(out)
}

/// Files a worktree changed from the fleet's base: committed on its branch, staged,
/// unstaged, or new.
fn changed_files(wt: &Path, base: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    if let Some(t) = git_out(wt, &["diff", "--name-only", base]) {
        files.extend(t.lines().map(String::from));
    }
    if let Some(t) = git_out(
        wt,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
    ) {
        files.extend(t.lines().map(String::from));
    }
    files.sort();
    files.dedup();
    files
}

/// The merge plan, and with `yes`, a commit of each finished worktree's changes on its
/// own branch. tend never merges into the user's branch: it prints the commands.
fn merge_plan(dir: &Path, meta: &Meta, model: &Model, yes: bool) -> Result<String> {
    use tend_core::fleet::model::TaskState;
    let mut out = String::new();
    let (Some(repo), Some(base)) = (&meta.repo, &meta.base) else {
        return Ok(out);
    };
    let order: Vec<String> = model.tasks().map(|t| t.id.clone()).collect();
    // Each worktree, with its slot's tasks, in the order the first of them was added.
    let mut rows: Vec<MergeRow> = Vec::new();
    let mut not_picked = 0;
    let mut in_parent = 0;
    for e in fs::read_dir(dir.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let wt = e.path();
        let name = e.file_name().to_string_lossy().replace("__", "/");
        let files = changed_files(&wt, base);
        if files.is_empty() {
            continue;
        }
        // A worktree's tasks: its slot's, or for a team worktree (`team-<name>`), those of
        // every slot whose role works in that team.
        let team = name.strip_prefix("team-").map(|t| Workspace::Team {
            name: t.to_string(),
        });
        let mine: Vec<&tend_core::fleet::model::Task> = model
            .tasks()
            .filter(|t| {
                t.slot.as_deref().is_some_and(|s| {
                    s == name
                        || team
                            .as_ref()
                            .is_some_and(|ws| model.role_of(s).is_some_and(|r| r.workspace == *ws))
                })
            })
            .collect();
        let first = mine
            .iter()
            .filter_map(|t| order.iter().position(|o| *o == t.id))
            .min()
            .unwrap_or(usize::MAX);
        // A part whose work went up into its parent's worktree is merged with it.
        let lifted_into = |t: &tend_core::fleet::model::Task| {
            let p = model.tasks().find(|p| Some(&p.id) == t.parent.as_ref())?;
            if p.best_of
                || p.target.as_deref() == Some("judge")
                || p.slot.as_deref() == Some(name.as_str())
            {
                return None;
            }
            p.slot.clone()
        };
        if !mine.is_empty()
            && mine
                .iter()
                .all(|t| t.state == TaskState::Done && lifted_into(t).is_some())
        {
            in_parent += 1;
            continue;
        }
        // A best-of candidate the judge did not pick is not merged (G9).
        if !mine.is_empty() && mine.iter().all(|t| t.picked == Some(false)) {
            not_picked += 1;
            continue;
        }
        let finished = !mine.is_empty() && mine.iter().all(|t| t.state == TaskState::Done);
        let labels = mine
            .iter()
            .map(|t| format!("{} {}", t.id, format!("{:?}", t.state).to_lowercase()))
            .collect();
        let tasks = mine.iter().map(|t| t.id.clone()).collect();
        rows.push(MergeRow {
            first,
            name,
            wt,
            files,
            labels,
            tasks,
            finished,
        });
    }
    rows.sort_by(|a, b| (a.first, &a.name).cmp(&(b.first, &b.name)));
    if in_parent > 0 {
        let _ = writeln!(
            out,
            "Left out: {in_parent} worktree{} of parts whose work is in their lead's worktree.",
            if in_parent == 1 { "" } else { "s" }
        );
    }
    if not_picked > 0 {
        let _ = writeln!(
            out,
            "Left out: {not_picked} worktree{} of candidates the judge did not pick.",
            if not_picked == 1 { "" } else { "s" }
        );
    }
    if rows.is_empty() {
        let _ = writeln!(out, "Merge plan: no worktree changed anything.");
        return Ok(out);
    }
    let _ = writeln!(
        out,
        "Merge plan ({} worktree{} with changes):",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    let mut by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (
        i,
        MergeRow {
            name,
            files,
            labels,
            finished,
            ..
        },
    ) in rows.iter().enumerate()
    {
        let shown: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        let more = if files.len() > 4 {
            format!(" and {} more", files.len() - 4)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {}. {name}{}  {}{more}{}",
            i + 1,
            if labels.is_empty() {
                String::new()
            } else {
                format!(" ({})", labels.join(", "))
            },
            shown.join(", "),
            if *finished {
                ""
            } else {
                "  [not finished: left out]"
            }
        );
        for f in files {
            by_file.entry(f.as_str()).or_default().push(name.as_str());
        }
    }
    for (f, names) in &by_file {
        if names.len() > 1 {
            let _ = writeln!(
                out,
                "  ! {} all change {f}: merge one, then rebase the next on it.",
                names.join(", ")
            );
        }
    }
    if !yes {
        let _ = writeln!(out, "`tend fleet merge --yes` commits each finished worktree's changes to its own branch, and prints the merge commands.");
        return Ok(out);
    }
    let mut merges = Vec::new();
    for MergeRow {
        name,
        wt,
        labels,
        tasks,
        finished,
        ..
    } in &rows
    {
        if !finished {
            continue;
        }
        let branch = branch_name(&meta.id, name);
        let dirty = git_out(wt, &["status", "--porcelain"]).is_some();
        if dirty {
            let summary: Vec<String> = model
                .tasks()
                .filter(|t| tasks.contains(&t.id))
                .map(|t| {
                    format!(
                        "{}: {}",
                        t.id,
                        t.outcome.clone().unwrap_or_else(|| t.title.clone())
                    )
                })
                .collect();
            let msg = format!(
                "{}\n\n{}\n\ntend fleet {}, {name}.",
                labels.join(", "),
                summary.join("\n"),
                meta.id
            );
            let run = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(wt)
                    .args(args)
                    .stdin(Stdio::null())
                    .output()
            };
            let markers = Command::new("git")
                .arg("-C")
                .arg(wt)
                .args([
                    "grep",
                    "-l",
                    "--untracked",
                    "-e",
                    "^<<<<<<< ",
                    "-e",
                    "^>>>>>>> ",
                ])
                .stdin(Stdio::null())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if !markers.is_empty() {
                let _ = writeln!(
                    out,
                    "  not committing {name}: conflict markers in {}. Resolve them first.",
                    markers.lines().collect::<Vec<_>>().join(", ")
                );
                continue;
            }
            let failed = [&["add", "-A"][..], &["commit", "-q", "-m", &msg]]
                .into_iter()
                .find_map(|args| match run(args) {
                    Ok(o) if o.status.success() => None,
                    Ok(o) => Some(
                        String::from_utf8_lossy(&o.stderr)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Err(e) => Some(e.to_string()),
                });
            if let Some(why) = failed {
                let _ = writeln!(out, "  could not commit {name}'s changes: {why}");
                continue;
            }
            let _ = writeln!(out, "  committed {name}'s changes on {branch}.");
        }
        merges.push(format!("git merge {branch}"));
    }
    if !merges.is_empty() {
        let _ = writeln!(
            out,
            "Merge them into your branch, in this order, in {repo}:"
        );
        for m in merges {
            let _ = writeln!(out, "  {m}");
        }
    }
    Ok(out)
}

/// What moving a task's work did: whether anything changed, and the files left with
/// conflict markers.
#[derive(Debug, Default)]
struct Moved {
    moved: bool,
    conflicts: Vec<String>,
}

/// One worktree in the merge plan.
struct MergeRow {
    /// Where its slot's first task is on the board: the merge order.
    first: usize,
    name: String,
    wt: PathBuf,
    files: Vec<String>,
    labels: Vec<String>,
    /// Its tasks' ids (a team worktree has several slots' tasks).
    tasks: Vec<String>,
    /// Every task of its slot is done.
    finished: bool,
}

/// `tend fleet merge [--yes]`.
pub fn merge(spec: Option<&str>, yes: bool) -> Result<String> {
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    if yes && is_up(&dir) {
        bail!("the fleet is up; take it down (`tend fleet down`) before committing its worktrees");
    }
    let model = replay(&meta, &read_ledger(&dir));
    merge_plan(&dir, &meta, &model, yes)
}

/// `tend fleet replay`: the recorded events through the model again, with the fleet's
/// own file (the records must come out the same) or another one (what it would have
/// decided, given the same events) (G7).
pub fn replay_cmd(spec: Option<&str>, file: Option<&Path>) -> Result<String> {
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    let records = read_ledger(&dir);
    let other = match file {
        Some(f) => Some(load_spec(Some(f), None)?.0),
        None => None,
    };
    let mut m = Model::new(other.clone().unwrap_or_else(|| meta.spec.clone()));
    let mut again = Vec::new();
    for r in &records {
        if let FleetBody::Event { event } = &r.body {
            again.extend(m.step(event.clone(), r.ts_ms));
        }
    }
    let mut out = String::new();
    let tally = |rs: &[FleetRecord]| {
        let mut t: BTreeMap<String, usize> = BTreeMap::new();
        for r in rs {
            if let FleetBody::Command { command, .. } = &r.body {
                let kind = match command {
                    Cmd::Answer { text, .. } if text.starts_with("tend: refused") => {
                        "refusal".to_string()
                    }
                    c => serde_json::to_value(c)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                        .unwrap_or_default(),
                };
                *t.entry(kind).or_default() += 1;
            }
        }
        t
    };
    let first_diff = records
        .iter()
        .zip(&again)
        .position(|(a, b)| a != b)
        .or((records.len() != again.len()).then(|| records.len().min(again.len())));
    match (&other, first_diff) {
        (None, None) => {
            let _ = writeln!(
                out,
                "The ledger's {} events replay to the same {} records.",
                records
                    .iter()
                    .filter(|r| matches!(r.body, FleetBody::Event { .. }))
                    .count(),
                records.len()
            );
        }
        (None, Some(i)) => {
            let _ = writeln!(out, "The replay differs from the ledger at record {i}:");
            let show = |r: Option<&FleetRecord>| {
                r.and_then(|r| serde_json::to_string(r).ok())
                    .unwrap_or_else(|| "(none)".into())
            };
            let _ = writeln!(
                out,
                "  ledger: {}",
                tend_core::text::truncate_chars(&show(records.get(i)), 300)
            );
            let _ = writeln!(
                out,
                "  replay: {}",
                tend_core::text::truncate_chars(&show(again.get(i)), 300)
            );
        }
        (Some(_), _) => {
            let (a, b) = (tally(&records), tally(&again));
            let kinds: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            let _ = writeln!(
                out,
                "Commands, as recorded and with {}:",
                file.map(|f| f.display().to_string()).unwrap_or_default()
            );
            for k in kinds {
                let (x, y) = (
                    a.get(k).copied().unwrap_or(0),
                    b.get(k).copied().unwrap_or(0),
                );
                let _ = writeln!(
                    out,
                    "  {k:<12} {x:>4} → {y:<4}{}",
                    if x != y { "  *" } else { "" }
                );
            }
            let _ = writeln!(out, "Given the same events; sessions the other file would have started differently could have acted differently.");
        }
    }
    Ok(out)
}

/// `tend fleet restart`: start a fleet's hub again from its ledger (G1).
pub fn restart(spec: Option<&str>) -> Result<String> {
    let dir = resolve(spec)?;
    if is_up(&dir) {
        bail!("the fleet's hub is running; `tend fleet status` shows it");
    }
    let exe = std::env::current_exe().context("find tend executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hub.log"))?;
    Command::new(exe)
        .args(["fleet", "hub", "--resume", "--dir"])
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("start the fleet hub")?;
    for _ in 0..200 {
        if is_up(&dir) {
            return status(dir.file_name().and_then(|n| n.to_str()));
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "the fleet hub did not start; see {}",
        dir.join("hub.log").display()
    )
}

/// Fleet control is the user's: refuse it in a fleet session's shell. The hub also
/// tells sessions apart by their process tree.
fn refuse_in_fleet_session() -> Result<()> {
    if std::env::var_os("TEND_FLEET_SLOT").is_some() {
        bail!("Fleet control is the user's. A fleet session asks its lead or the user.");
    }
    Ok(())
}

/// `tend fleet down`: stop every session, and wait for the hub to finish.
pub fn down(spec: Option<&str>) -> Result<String> {
    refuse_in_fleet_session()?;
    let dir = resolve(spec)?;
    let said =
        send(&dir, &HubRequest::Down).map_err(|_| anyhow!("the fleet's hub is not running"))?;
    // Refused (a fleet session asked): nothing is going down, so do not wait for it.
    if said.starts_with("tend: refused") {
        bail!("{}", said.trim_start_matches("tend: "));
    }
    for _ in 0..400 {
        if !is_up(&dir) {
            return Ok(format!("{said}\nThe fleet is down."));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(format!(
        "{said}\nThe hub is still stopping sessions; check `tend fleet status`."
    ))
}

/// A request to the fleet's hub, for stop, spawn, and scale.
pub fn ask(spec: Option<&str>, req: HubRequest) -> Result<String> {
    refuse_in_fleet_session()?;
    let dir = resolve(spec)?;
    send(&dir, &req).map_err(|_| anyhow!("the fleet's hub is not running"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_and_the_usage_limit_come_from_result_lines() {
        let line = r#"{"type":"result","subtype":"success","total_cost_usd":0.0035,"usage":{"input_tokens":2,"cache_creation_input_tokens":242,"cache_read_input_tokens":22345,"output_tokens":5},"result":"ok"}"#;
        assert_eq!(
            usage_of(line, "w-1"),
            Some(FleetEvent::Usage {
                slot: "w-1".into(),
                tokens: 22594,
                cost_usd: 0.0035
            })
        );
        assert!(limit_of(line, "w-1").is_none());
        let hit = r#"{"type":"result","is_error":true,"result":"Claude AI usage limit reached|1792630000"}"#;
        assert!(matches!(
            limit_of(hit, "w-1"),
            Some(FleetEvent::UsageLimit { .. })
        ));
        // An agent's long answer that mentions usage limits is not one.
        let talk = format!(
            r#"{{"type":"result","result":"{}"}}"#,
            "the API has a usage limit; ".repeat(20)
        );
        assert!(limit_of(&talk, "w-1").is_none());
        // A short answer that mentions a limit is not one either (dogfood review).
        let short =
            r#"{"type":"result","result":"Done. Note: the API has a usage limit of 100/min."}"#;
        assert!(limit_of(short, "w-1").is_none());
        let reached = r#"{"type":"result","result":"You've hit your session limit · resets 4pm"}"#;
        assert!(limit_of(reached, "w-1").is_some());
    }

    #[test]
    fn an_exact_fleet_id_wins_over_longer_ids_it_starts() {
        let dirs = vec![PathBuf::from("/f/100-1"), PathBuf::from("/f/100-12")];
        assert_eq!(pick(&dirs, "100-1").unwrap(), PathBuf::from("/f/100-1"));
        assert!(pick(&dirs, "100-").is_err());
    }

    #[test]
    fn a_check_that_leaves_a_background_process_still_finishes() {
        let d = std::env::temp_dir();
        let t = std::time::Instant::now();
        let (code, out) = run_with_timeout(&d, "sleep 30 & echo hi", Duration::from_secs(60));
        assert_eq!(code, Some(0), "{out}");
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "waited for the leftover"
        );
    }

    #[test]
    fn a_check_with_a_lot_of_output_still_finishes() {
        let d = std::env::temp_dir();
        let (code, out) = run_with_timeout(
            &d,
            "head -c 6000000 /dev/zero | tr '\\0' x; echo done",
            Duration::from_secs(20),
        );
        assert_eq!(code, Some(0), "{}", out.len());
    }

    #[test]
    fn output_that_is_not_utf8_does_not_stop_the_reader() {
        let d = std::env::temp_dir().join(format!("tend-readout-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let log = File::create(d.join("o.jsonl")).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut input = b"\xff\xfe junk\n".to_vec();
        input.extend(br#"{"type":"result","result":"ok"}"#);
        input.push(b'\n');
        read_output(&input[..], log, "w-1", &tx);
        assert!(rx.try_iter().any(|m| matches!(m, Msg::TurnEnded(_))));
    }

    #[test]
    fn a_sessions_unread_messages_are_those_its_ledger_has_not_seen() {
        let d = std::env::temp_dir().join(format!("tend-unread-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("in.jsonl");
        fs::write(&inbox, "{}\n{}\n{}\n").unwrap();
        let seen = tend_core::Entry {
            seq: 1,
            ts_ms: 0,
            causes: vec![],
            body: tend_core::Body::Event {
                event: tend_core::Event::Message {
                    from: "lead-1".into(),
                    kind: "answer".into(),
                    text: "x".into(),
                },
            },
        };
        assert_eq!(unread_for_session(&inbox, &[seen]), 2);
        assert_eq!(unread_for_session(&d.join("missing"), &[]), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn uuids_are_version_4() {
        let u = uuid_v4().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
        assert_ne!(u, uuid_v4().unwrap());
    }

    #[test]
    fn fleet_files_parse_from_toml() {
        let d = std::env::temp_dir().join(format!("tend-hub-toml-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let f = d.join("fleet.toml");
        fs::write(
            &f,
            r#"
preset = "lead"
check  = "make check"

[role.lead]
model = "opus"

[role.worker]
model         = "sonnet"
replicas      = "1..3"
may_not_write = ["tests/**"]
budget        = { tokens = "2M", wall = "45m", attempts = 3 }

[limits]
sessions = 5
tokens   = "20M"
"#,
        )
        .unwrap();
        let (spec, _) = load_spec(Some(&f), None).unwrap();
        assert_eq!(spec.roles["worker"].max, 3);
        assert_eq!(spec.limits.sessions, 5);
        fs::write(
            &f,
            "preset = \"lead\"\n[role.worker]\nreplicas = \"9..1\"\n",
        )
        .unwrap();
        let e = load_spec(Some(&f), None).unwrap_err().to_string();
        assert!(
            e.contains("1 problem") && e.contains("9..1 is empty"),
            "{e}"
        );
        fs::write(&f, "preset = \"lead\"\nrolez = 3\n").unwrap();
        let e = load_spec(Some(&f), None).unwrap_err().to_string();
        assert!(e.contains("rolez"), "{e}");
        assert!(load_spec(None, None).is_err(), "nothing to run");
        let _ = fs::remove_dir_all(&d);
    }
}
