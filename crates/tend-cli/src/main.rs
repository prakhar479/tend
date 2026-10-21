//! tend CLI: launcher, hook shim, session daemon, and inspection commands.

// Unix sockets, process groups, and signals throughout: say so instead of a page of
// errors (I5, X1).
#[cfg(not(unix))]
compile_error!("tend supports Unix (Linux, macOS) only for now");

mod audit;
mod config;
mod connector;
mod daemon;
mod doctor;
mod export;
mod fleet;
mod hub;
mod ipc;
mod launcher;
mod paths;
mod plugin;
mod report;
mod retro;
mod run;
mod sessions;
mod shim;
mod store;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tend_core::Mode;

#[derive(Parser)]
#[command(
    name = "tend",
    version,
    about = "Local supervisor for command-line coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start Claude Code under tend. Other arguments, `--help` included, go to `claude`.
    /// `--tend-mode` may come anywhere.
    #[command(disable_help_flag = true)]
    Claude {
        #[arg(long = "tend-mode", value_enum, default_value = "look")]
        mode: ModeArg,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start Codex under tend (Phase 2).
    #[command(hide = true)]
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install stable hooks for an agent once (Phase 2: codex).
    #[command(hide = true)]
    Init { agent: String },
    /// Change the mode of a live session.
    Mode {
        #[arg(value_enum)]
        mode: ModeArg,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Allow one edit the test guard blocked (the latest, or action a-N). Run it yourself,
    /// for example with `! tend allow` in Claude Code; the agent may not.
    Allow {
        id: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show what happened in a session (default: the newest one started here).
    Report {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Every section, including empty ones.
        #[arg(long)]
        full: bool,
    },
    /// Print the full original of a summarized output (o-N).
    Show {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show the cause chain of an action (a-N), output (o-N), or entry number.
    Why {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Note that tend got something wrong: an action (a-N), an output (o-N), an entry
    /// number, or `session`. `tend review` lists the notes.
    Flag {
        target: String,
        #[arg(required = true, num_args = 1..)]
        note: Vec<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// List flags across all sessions, with what each one points at.
    Review,
    /// Check the install: which tend runs, the Claude Code version, the data dir, config.
    Doctor,
    /// One session as a sanitized JSON bundle (paths, home, user name, and secrets
    /// replaced), to attach to a bug report.
    Export {
        /// An id, a prefix, `-N`, or a path (default: the newest session started here).
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Leave out the stored tool outputs (keep only the ledger).
        #[arg(long)]
        no_outputs: bool,
    },
    /// Delete recorded sessions (their ledgers and stored outputs). Live sessions are
    /// kept.
    Purge {
        /// One session: an id, a prefix, `-N`, or a path.
        #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["all", "older_than"])]
        session: Option<String>,
        /// Every session.
        #[arg(long)]
        all: bool,
        /// Sessions with no activity for this many days.
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// (Experimental) What a git diff, or a tend session, changed in tests and checks:
    /// assertions and tests, snapshots, suppressions, check config, dependencies.
    Audit {
        /// `A..B`, `A...B` (from the merge base), or `A` (to the working tree).
        /// Default: HEAD to the working tree.
        range: Option<String>,
        #[arg(long, value_enum, default_value = "text")]
        format: AuditFormat,
        /// The audit of a tend session (with `experimental_audit = true`): an id, a
        /// prefix, or `-N`; alone, the newest.
        #[arg(long, num_args = 0..=1, default_missing_value = "-1", allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// One line for the Claude Code status line: mode and what tend did so far.
    /// Prints nothing outside a tend session.
    Status,
    /// A fleet's task board: add, give out, take, finish, or block tasks. In a fleet
    /// session, the agent uses these; tend knows which session asks.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
        /// A fleet id or prefix (default: this session's fleet, or the newest up).
        #[arg(long, global = true)]
        fleet: Option<String>,
    },
    /// Send a message in a fleet: to `user`, `@role`, `#team` (the sessions sharing a
    /// team worktree), or a session. Without --kind: an answer to a question, else the
    /// first kind the fleet's channels allow.
    Msg {
        to: String,
        text: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Messages: this fleet session's, or (for you) the fleet's inbox.
    Inbox {
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Fleets of agents: `tend fleet up` runs one from a fleet file. Alone, it lists the
    /// fleets and the live tend sessions on this machine, grouped by repo.
    Fleet {
        #[command(subcommand)]
        cmd: Option<FleetCmd>,
    },
    /// Reverse an action (Phase 3).
    #[command(hide = true)]
    Undo { id: String },
    /// Run look mode over an existing Claude Code transcript, or over all of them.
    Retro {
        #[arg(required_unless_present = "all")]
        transcript: Option<PathBuf>,
        /// Every session transcript in the Claude Code projects dir.
        #[arg(long, conflicts_with = "transcript")]
        all: bool,
        /// Projects dir for --all (default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects).
        #[arg(long, requires = "all")]
        projects: Option<PathBuf>,
        /// With --all, keep projects whose working directory was under /tmp (scratch work,
        /// such as tend's own test runs). They are left out by default.
        #[arg(long, requires = "all")]
        include_tmp: bool,
        /// For one transcript: every section, including empty ones.
        #[arg(long)]
        full: bool,
        /// With --all: totals only, no paths or session names, safe to share.
        #[arg(long, requires = "all")]
        summary: bool,
    },
    /// List recorded sessions.
    Sessions {
        /// Delete sessions with no activity for this many days (live sessions are kept).
        #[arg(long, value_name = "DAYS")]
        prune: Option<u64>,
    },
    /// Wrapper tend puts around a test command (see run.rs). Keeps the exit code.
    #[command(hide = true)]
    Run {
        #[arg(long)]
        id: String,
        #[arg(long)]
        sh: String,
    },
    /// Hook entry point. The agent calls this. Always exits 0.
    #[command(hide = true)]
    Hook { event: String },
    /// Session daemon. The launcher starts this.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        session_dir: PathBuf,
        #[arg(long)]
        parent_pid: Option<i32>,
        #[arg(long, value_enum, default_value = "look")]
        mode: ModeArg,
        #[arg(long, default_value = "claude-code")]
        agent: String,
        #[arg(long)]
        idle_exit_minutes: Option<u64>,
    },
}

#[derive(Subcommand)]
enum FleetCmd {
    /// Start a fleet in this repo, from a fleet file (TOML) or a preset.
    Up {
        /// The fleet file. See docs/fleet-topologies-2026-10-21.md.
        file: Option<PathBuf>,
        /// solo, peers, lead, pipeline, review, best-of, specialists, pool, tree.
        #[arg(long)]
        preset: Option<String>,
        /// The tend mode of the fleet's sessions.
        #[arg(long, value_enum, default_value = "look")]
        mode: ModeArg,
    },
    /// A fleet's sessions: state, pid, turns, restarts, worktree.
    Status {
        /// A fleet id or prefix (default: the newest one that is up).
        fleet: Option<String>,
    },
    /// Stop one session of a fleet (it is not restarted).
    Stop {
        slot: String,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Start another session of a role (within its maximum).
    Spawn {
        role: String,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Run `n` sessions of a stream role.
    Scale {
        role: String,
        n: u32,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop every session and the hub.
    Down { fleet: Option<String> },
    /// What a fleet did: tasks, sessions, spend, and the merge plan.
    Report { fleet: Option<String> },
    /// The order to merge finished worktrees in, and where they conflict. With `--yes`
    /// (fleet down), commit each finished worktree's changes on its own branch.
    Merge {
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop handing out tasks and starting sessions; running turns finish.
    Pause { fleet: Option<String> },
    /// Hand out work again (after `pause`, a budget, or the usage limit).
    Resume { fleet: Option<String> },
    /// Remove worktrees of fleets that are down and have nothing unsaved in them.
    /// Without `--yes`, only lists them.
    Prune {
        #[arg(long)]
        yes: bool,
    },
    /// The hub process (started by `up`).
    #[command(hide = true)]
    Hub {
        #[arg(long)]
        dir: PathBuf,
        /// Start again from the ledger (`tend fleet restart`).
        #[arg(long)]
        resume: bool,
    },
    /// Start a fleet's hub again after it stopped: stream sessions resume their
    /// conversations, and their tasks go back to the queue.
    Restart { fleet: Option<String> },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Put a task on the board. With one role that may take it, it goes to that role.
    Add {
        title: String,
        /// More detail for the session that does it.
        #[arg(long, default_value = "")]
        body: String,
        /// Tasks that must be done first, comma-separated (t-1,t-2).
        #[arg(long, value_delimiter = ',')]
        depends: Vec<String>,
        /// Paths the task expects to touch (repeat for more).
        #[arg(long)]
        scope: Vec<String>,
        /// The role that should do it.
        #[arg(long)]
        role: Option<String>,
        /// The command that decides it is done (default: the fleet file's `check`).
        #[arg(long)]
        check: Option<String>,
    },
    /// Give a task to a role or a session.
    Assign { task: String, to: String },
    /// Take a ready task: the given one, or the next one for this session's role.
    Claim { task: Option<String> },
    /// Finish a task, with a one-line summary.
    Done { task: String, summary: String },
    /// Say a task cannot be done, and why. Its creator is told.
    Blocked { task: String, reason: String },
    /// Send a task under review back to work, and say why (its creator or the user).
    Reject { task: String, reason: String },
    /// The board.
    List,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuditFormat {
    Text,
    Markdown,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Look,
    Optimize,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Look => Mode::Look,
            ModeArg::Optimize => Mode::Optimize,
        }
    }
}

fn fleet_cmd(c: FleetCmd) -> Result<i32> {
    let text = match c {
        FleetCmd::Up { file, preset, mode } => {
            let preset = match preset {
                Some(p) => Some(
                    serde_json::from_value(serde_json::Value::String(p.clone()))
                        .map_err(|_| anyhow::anyhow!("no preset `{p}`"))?,
                ),
                None => None,
            };
            let id = hub::up(file.as_deref(), preset, mode.into())?;
            format!(
                "fleet {id} is up.\n{}",
                hub::status(Some(&id)).unwrap_or_default()
            )
        }
        FleetCmd::Status { fleet } => hub::status(fleet.as_deref())?,
        FleetCmd::Stop { slot, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Stop { slot })?
        }
        FleetCmd::Spawn { role, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Spawn { role })?
        }
        FleetCmd::Scale { role, n, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Scale { role, n })?
        }
        FleetCmd::Down { fleet } => hub::down(fleet.as_deref())?,
        FleetCmd::Report { fleet } => hub::report(fleet.as_deref())?,
        FleetCmd::Merge { yes, fleet } => hub::merge(fleet.as_deref(), yes)?,
        FleetCmd::Pause { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Pause)?,
        FleetCmd::Resume { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Resume)?,
        FleetCmd::Prune { yes } => hub::prune(yes)?,
        FleetCmd::Hub { dir, resume } => {
            hub::run_hub(&dir, resume)?;
            return Ok(0);
        }
        FleetCmd::Restart { fleet } => hub::restart(fleet.as_deref())?,
    };
    println!("{}", text.trim_end());
    Ok(0)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // The shim must never fail the agent: handle it before clap, whose errors exit 2, and
    // exit 2 from a PreToolUse hook blocks the tool call. Extra or missing arguments (a
    // newer settings file, an older binary) still exit 0 (rule 1).
    if argv.get(1).map(String::as_str) == Some("hook") {
        let event = argv.get(2).cloned().unwrap_or_default();
        // `--plugin`: called by the Claude Code plugin, with no launcher (plugin.rs).
        let from_plugin = argv.iter().skip(3).any(|a| a == "--plugin");
        // Exit 0 even if something panics.
        let _ = std::panic::catch_unwind(|| {
            if from_plugin {
                plugin::run(&event)
            } else {
                shim::run(&event)
            }
        });
        std::process::exit(0);
    }
    // `tend reap <pgid>`: the watchdog `tend run` starts (see run.rs). No clap: it must
    // start fast and never fail loudly.
    if argv.get(1).map(String::as_str) == Some("reap") {
        std::process::exit(run::reap(argv.get(2).map_or("", String::as_str)));
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // `tend run` stands in for the agent's command: if its other arguments do not
            // parse, still run the command, unfiltered.
            if argv.get(1).map(String::as_str) == Some("run") {
                if let Some(sh) = arg_value(&argv, "--sh") {
                    std::process::exit(run::run("", &sh));
                }
            }
            e.exit()
        }
    };
    if let Cmd::Run { id, sh } = &cli.cmd {
        std::process::exit(run::run(id, sh));
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("tend: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The value of `--name value` or `--name=value` in raw arguments.
fn arg_value(argv: &[String], name: &str) -> Option<String> {
    let eq = format!("{name}=");
    argv.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            argv.get(i + 1).cloned()
        } else {
            a.strip_prefix(&eq).map(str::to_string)
        }
    })
}

/// Every flag, grouped by session, with the flagged entry and how to look closer.
fn review(sessions: &[PathBuf]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (mut flags, mut with) = (0, 0);
    for dir in sessions {
        let list = store::read_flags(dir);
        if list.is_empty() {
            continue;
        }
        flags += list.len();
        with += 1;
        let entries = store::read_entries(dir).unwrap_or_default();
        let label = dir
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        let _ = writeln!(out, "Session {label}");
        for f in &list {
            let what = match report::find_target(&entries, &f.target) {
                Some(e) => report::describe_entry(e),
                None => format!("{} (whole session)", f.target),
            };
            let _ = writeln!(out, "  - {}", short(&what));
            let _ = writeln!(out, "    note: {}", f.note);
            let look = if f.target.starts_with("o-") {
                format!("tend show {} --session {}", f.target, dir.display())
            } else if f.target == "session" {
                format!("tend report --session {}", dir.display())
            } else {
                format!("tend why {} --session {}", f.target, dir.display())
            };
            let _ = writeln!(out, "    look: {look}");
        }
    }
    format!("{flags} flags in {with} sessions\n{out}")
}

fn short(s: &str) -> String {
    tend_core::text::truncate_chars(s, 200)
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Claude { mode, args } => launcher::run_claude(mode.into(), args),
        Cmd::Codex { .. } => bail!("the Codex connector is Phase 2 (see docs/TASKS.md)"),
        Cmd::Init { agent } => bail!("`tend init {agent}` is Phase 2 (see docs/TASKS.md)"),
        Cmd::Undo { .. } => bail!("undo is Phase 3 (see docs/TASKS.md)"),
        Cmd::Mode { mode, session } => {
            let dir = sessions::resolve(session.as_deref(), true)?;
            let req = ipc::Request::SetMode {
                mode: mode.into(),
                token: paths::read_token(&dir),
            };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))?;
            println!("{}", reply.message.unwrap_or_default());
            Ok(0)
        }
        Cmd::Allow { id, session } => {
            let dir = sessions::resolve(session.as_deref(), true)?;
            let req = ipc::Request::Allow {
                action: id,
                token: paths::read_token(&dir),
            };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
                .with_context(|| format!("session {} is not live", dir.display()))?;
            println!("{}", reply.message.unwrap_or_default());
            Ok(0)
        }
        Cmd::Report { session, full } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::read_entries(&dir)?;
            let label = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            print!("{}", report::render(&label, &entries, true, full));
            if let Some(a) = std::fs::read_to_string(dir.join("audit.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<tend_core::audit::Report>(&t).ok())
            {
                println!(
                    "{}: {} (`tend audit --session`)",
                    audit::LABEL,
                    a.one_line()
                );
            }
            let flags = store::read_flags(&dir);
            if full || !flags.is_empty() {
                println!("Flags: {}", flags.len());
            }
            for f in &flags {
                println!("  - {}: {}", f.target, f.note);
            }
            Ok(0)
        }
        Cmd::Show { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::read_entries(&dir)?;
            let hash = report::find_output_blob(&entries, &id)
                .ok_or_else(|| anyhow::anyhow!("no output {id} in {}", dir.display()))?;
            print!("{}", store::read_blob(&dir, &hash)?);
            Ok(0)
        }
        Cmd::Flag {
            target,
            note,
            session,
        } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            if target != "session" {
                let entries = store::read_entries(&dir)?;
                if report::find_target(&entries, &target).is_none() {
                    bail!("no entry `{target}` in {}", dir.display());
                }
            }
            let flag = store::Flag {
                ts_ms: paths::now_ms(),
                target: target.clone(),
                note: note.join(" "),
            };
            store::append_flag(&dir, &flag)?;
            println!("Flagged {target}. `tend review` lists all flags.");
            Ok(0)
        }
        Cmd::Status => {
            // A status line must never show an error: print nothing instead. Under the
            // launcher the session is in the environment; under the plugin, Claude Code
            // passes the session id on stdin.
            let dir = std::env::var_os("TEND_SESSION_DIR")
                .map(PathBuf::from)
                .or_else(|| {
                    use std::io::{IsTerminal, Read};
                    if std::io::stdin().is_terminal() {
                        return None;
                    }
                    let mut input = String::new();
                    std::io::stdin().read_to_string(&mut input).ok()?;
                    let v: serde_json::Value = serde_json::from_str(&input).ok()?;
                    plugin::existing_session_dir(v.get("session_id")?.as_str()?)
                });
            if let Some(dir) = dir {
                if let Ok(entries) = store::read_entries(&dir) {
                    println!("{}", report::status_line(&entries));
                }
            }
            Ok(0)
        }
        Cmd::Audit {
            range,
            format,
            session,
        } => {
            let (report, title) = match session {
                Some(spec) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    let text = std::fs::read_to_string(dir.join("audit.json")).with_context(|| {
                        format!(
                            "no audit in session {} (set `experimental_audit = true` in the config)",
                            dir.display()
                        )
                    })?;
                    let id = dir.file_name().map(|n| n.to_string_lossy().to_string());
                    (
                        serde_json::from_str(&text)?,
                        format!("session {}", id.unwrap_or_default()),
                    )
                }
                None => {
                    let root = audit::repo_root(&std::env::current_dir()?)?;
                    let scratch = std::env::temp_dir();
                    let (from, to) = audit::resolve_range(&root, range.as_deref(), &scratch)?;
                    let title = range.unwrap_or_else(|| "HEAD..working tree".into());
                    let policy = config::load_policy(&root).0;
                    (
                        audit::run(&root, &from, &to, &|p| policy.audit_ignores(p))?,
                        title,
                    )
                }
            };
            match format {
                AuditFormat::Text => print!("{}", audit::render_text(&report, &title)),
                AuditFormat::Markdown => print!("{}", audit::render_markdown(&report, &title)),
                AuditFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            }
            Ok(0)
        }
        Cmd::Export {
            session,
            no_outputs,
        } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            println!("{}", export::export(&dir, !no_outputs)?);
            Ok(0)
        }
        Cmd::Purge {
            session,
            all,
            older_than,
        } => {
            let root = paths::sessions_dir();
            let removed = match (session, all, older_than) {
                (Some(spec), _, _) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    if sessions::is_live(&dir) {
                        bail!("session {} is live; end it first", dir.display());
                    }
                    std::fs::remove_dir_all(&dir)
                        .with_context(|| format!("remove {}", dir.display()))?;
                    vec![dir]
                }
                (None, true, _) => sessions::prune(&root, 0, u64::MAX / 2)?,
                (None, false, Some(days)) => sessions::prune(&root, days, paths::now_ms() / 1000)?,
                (None, false, None) => {
                    bail!("say what to purge: --session ID, --older-than DAYS, or --all")
                }
            };
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Doctor => {
            let (report, healthy) = doctor::run();
            print!("{report}");
            Ok(if healthy { 0 } else { 1 })
        }
        Cmd::Review => {
            print!("{}", review(&paths::list_sessions()));
            Ok(0)
        }
        Cmd::Why { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::read_entries(&dir)?;
            print!("{}", report::why_across(&entries, &id, &fleet::peer_entry)?);
            Ok(0)
        }
        Cmd::Fleet { cmd: None } => {
            print!("{}{}", hub::list(), fleet::view_default());
            Ok(0)
        }
        Cmd::Fleet { cmd: Some(c) } => fleet_cmd(c),
        Cmd::Msg {
            to,
            text,
            kind,
            fleet,
        } => {
            let said = hub::msg(fleet.as_deref(), to, kind, text)?;
            println!("{}", said.trim_end());
            Ok(if said.starts_with("tend: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Inbox { fleet } => {
            println!("{}", hub::show_inbox(fleet.as_deref())?.trim_end());
            Ok(0)
        }
        Cmd::Task { cmd, fleet } => {
            let verb = match cmd {
                TaskCmd::Add {
                    title,
                    body,
                    depends,
                    scope,
                    role,
                    check,
                } => hub::TaskVerb::Add {
                    title,
                    body,
                    depends_on: depends,
                    scope,
                    role,
                    check,
                },
                TaskCmd::Assign { task, to } => hub::TaskVerb::Assign { task, to },
                TaskCmd::Claim { task } => hub::TaskVerb::Claim { task },
                TaskCmd::Done { task, summary } => hub::TaskVerb::Done { task, summary },
                TaskCmd::Blocked { task, reason } => hub::TaskVerb::Blocked { task, reason },
                TaskCmd::Reject { task, reason } => hub::TaskVerb::Reject { task, reason },
                TaskCmd::List => hub::TaskVerb::List,
            };
            let text = hub::task(fleet.as_deref(), verb)?;
            println!("{}", text.trim_end());
            // A refusal is an error to the caller (an agent's script, or the user).
            Ok(if text.starts_with("tend: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Retro {
            transcript,
            all,
            projects,
            include_tmp,
            full,
            summary,
        } => {
            match transcript {
                Some(t) if !all => print!("{}", retro::run(&t, full)?),
                _ => {
                    let root = projects.unwrap_or_else(paths::claude_projects_dir);
                    print!("{}", retro::run_all(&root, 5, include_tmp, summary)?);
                }
            }
            Ok(0)
        }
        Cmd::Sessions { prune: Some(days) } => {
            let now = paths::now_ms() / 1000;
            let removed = sessions::prune(&paths::sessions_dir(), days, now)?;
            for d in &removed {
                println!("removed {}", d.display());
            }
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Sessions { prune: None } => {
            print!("{}", sessions::list(&paths::sessions_dir()));
            Ok(0)
        }
        Cmd::Daemon {
            session_dir,
            parent_pid,
            mode,
            agent,
            idle_exit_minutes,
        } => {
            daemon::run(
                session_dir,
                parent_pid,
                mode.into(),
                &agent,
                idle_exit_minutes,
            )?;
            Ok(0)
        }
        Cmd::Hook { .. } | Cmd::Run { .. } => unreachable!("handled in main"),
    }
}
