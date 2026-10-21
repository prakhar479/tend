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
use std::process::{ChildStdin, Command, Stdio};
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
    stdin: Option<ChildStdin>,
    run: Run,
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
}

pub fn run_hub(dir: &Path) -> Result<()> {
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
    };
    eprintln!("tend fleet hub {}: up", hub.meta.id);
    hub.apply(FleetEvent::Up);
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
                    if let Some(p) = self.procs.get_mut(&slot) {
                        p.turns += 1;
                        // A oneshot does one task: its first turn is all of it.
                        if p.run == Run::Oneshot {
                            p.stdin = None;
                        }
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
                        kill(p.pid, libc::SIGKILL);
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
            HubRequest::Task { claimed_slot, .. } => claimed_slot.clone(),
            _ => None,
        };
        let by = self.actor(pid, claimed.as_deref());
        let user = by == Actor::User;
        let event = match req {
            HubRequest::Ping => return "pong".into(),
            HubRequest::Status => return self.status(),
            // Fleet control is the user's; a session asks through its role's verbs.
            HubRequest::Down | HubRequest::Scale { .. } if !user => {
                return "tend: refused. Fleet control is the user's.".into();
            }
            HubRequest::Stop { slot } => FleetEvent::StopRequest { by, slot },
            HubRequest::Down => FleetEvent::Down,
            HubRequest::Spawn { role } => FleetEvent::SpawnRequest { by, role },
            HubRequest::Scale { role, n } => FleetEvent::Scale { role, n },
            HubRequest::Task { verb, .. } => match verb {
                TaskVerb::List => return render_tasks(&self.model),
                TaskVerb::Add {
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                } => FleetEvent::TaskAdd {
                    by,
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                },
                TaskVerb::Assign { task, to } => FleetEvent::TaskAssign { by, task, to },
                TaskVerb::Claim { task } => FleetEvent::TaskClaim { by, task },
                TaskVerb::Done { task, summary } => FleetEvent::TaskDone { by, task, summary },
                TaskVerb::Blocked { task, reason } => FleetEvent::TaskBlocked { by, task, reason },
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
            for r in &recs {
                if let FleetBody::Command { command, .. } = &r.body {
                    self.run(command);
                }
            }
            if first.is_none() {
                first = Some(recs);
            }
        }
        first.unwrap_or_default()
    }

    fn run(&mut self, c: &Cmd) {
        match c {
            Cmd::Spawn {
                slot,
                run,
                workspace,
                model,
                permission_mode,
                brief,
                ..
            } => match self.spawn(slot, *run, workspace, model.as_deref(), permission_mode.as_deref(), brief) {
                Ok(session) => self.queue.push_back(FleetEvent::Started {
                    slot: slot.clone(),
                    session,
                }),
                Err(e) => {
                    eprintln!("tend fleet hub: start {slot}: {e:#}");
                    inbox(&self.dir, &format!("Could not start {slot}: {e:#}"));
                    self.queue.push_back(FleetEvent::Exited {
                        slot: slot.clone(),
                        code: Some(127),
                    });
                }
            },
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
                            kill(pid, sig);
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
                let from = match from {
                    Actor::User => "the user".to_string(),
                    Actor::Tend => "tend".to_string(),
                    Actor::Slot { id } => id.clone(),
                };
                let kind = format!("{kind:?}").to_lowercase();
                match to {
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
                &format!("Role {role} runs in your terminal. Start it with `tend claude` in this repo; joining a fleet from a terminal comes later (F4)."),
            ),
            // Answers go back with the request's reply; waits need nothing done.
            Cmd::Backoff { .. } | Cmd::TaskRetry { .. } | Cmd::Answer { .. } => {}
        }
    }

    /// Write one JSON line to a slot's stdin. False if it has none.
    fn write_line(&mut self, slot: &str, v: &serde_json::Value) -> bool {
        let Some(stdin) = self.procs.get_mut(slot).and_then(|p| p.stdin.as_mut()) else {
            return false;
        };
        let mut line = v.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .is_ok()
    }

    fn spawn(
        &mut self,
        slot: &str,
        run: Run,
        workspace: &Workspace,
        model: Option<&str>,
        permission_mode: Option<&str>,
        brief: &str,
    ) -> Result<String> {
        let cwd = self.workspace_dir(slot, workspace)?;
        let session = uuid_v4()?;
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
            .args(["--session-id", &session])
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(err)
            .process_group(0)
            .spawn()
            .context("start tend claude")?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take();
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
        self.procs.insert(
            slot.to_string(),
            Proc {
                pid,
                stdin,
                run,
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

    fn status(&self) -> String {
        let mut out = render_status(&self.meta, &self.model, &self.procs, true);
        out.push_str(&render_tasks(&self.model));
        out
    }
}

/// Copy a slot's output to its log, and tell the hub about activity and turn ends.
fn read_output(out: impl Read, mut log: File, slot: &str, tx: &Sender<Msg>) {
    let mut last = 0;
    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        let _ = writeln!(log, "{line}");
        let kind = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from));
        let sent = if kind.as_deref() == Some("result") {
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

fn branch_name(fleet: &str, name: &str) -> String {
    format!("tend/{fleet}/{name}")
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

/// Send a request to a fleet's hub; its reply message.
pub fn send(dir: &Path, req: &HubRequest) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path(dir))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut out = String::new();
    BufReader::new(stream).read_line(&mut out)?;
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

/// A fleet by id or unique prefix; else the newest one that is up; else the newest.
pub fn resolve(spec: Option<&str>) -> Result<PathBuf> {
    let dirs = fleet_dirs();
    if let Some(s) = spec {
        let hits: Vec<&PathBuf> = dirs
            .iter()
            .filter(|d| {
                d.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(s))
            })
            .collect();
        return match hits[..] {
            [one] => Ok(one.clone()),
            [] => bail!("no fleet `{s}` (`tend fleet status` lists them)"),
            _ => bail!("`{s}` matches {} fleets; give more of the id", hits.len()),
        };
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
    out
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
        let _ = writeln!(
            out,
            "fleet {} · {} · {} sessions running · {}",
            meta.id,
            if is_up(&d) { "up" } else { "down" },
            live,
            meta.repo.as_deref().unwrap_or(&meta.cwd)
        );
    }
    out
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
            let ahead = git_out(repo, &["rev-list", "--count", &format!("{base}..{branch}")])
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
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

/// `tend fleet down`: stop every session, and wait for the hub to finish.
pub fn down(spec: Option<&str>) -> Result<String> {
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
    let dir = resolve(spec)?;
    send(&dir, &req).map_err(|_| anyhow!("the fleet's hub is not running"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
