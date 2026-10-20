//! tend CLI: launcher, hook shim, session daemon, and inspection commands.

mod config;
mod connector;
mod daemon;
mod ipc;
mod launcher;
mod paths;
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
    /// One line for the Claude Code status line: mode and what tend did so far.
    /// Prints nothing outside a tend session.
    Status,
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
    },
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

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // The shim must never fail the agent: handle it before clap, whose errors exit 2, and
    // exit 2 from a PreToolUse hook blocks the tool call. Extra or missing arguments (a
    // newer settings file, an older binary) still exit 0 (rule 1).
    if argv.get(1).map(String::as_str) == Some("hook") {
        let event = argv.get(2).cloned().unwrap_or_default();
        // Exit 0 even if something panics.
        let _ = std::panic::catch_unwind(|| shim::run(&event));
        std::process::exit(0);
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
            // A status line must never show an error: print nothing instead.
            if let Some(dir) = std::env::var_os("TEND_SESSION_DIR") {
                if let Ok(entries) = store::read_entries(&PathBuf::from(dir)) {
                    println!("{}", report::status_line(&entries));
                }
            }
            Ok(0)
        }
        Cmd::Review => {
            print!("{}", review(&paths::list_sessions()));
            Ok(0)
        }
        Cmd::Why { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::read_entries(&dir)?;
            print!("{}", report::why(&entries, &id)?);
            Ok(0)
        }
        Cmd::Retro {
            transcript,
            all,
            projects,
            include_tmp,
            full,
        } => {
            match transcript {
                Some(t) if !all => print!("{}", retro::run(&t, full)?),
                _ => {
                    let root = projects.unwrap_or_else(paths::claude_projects_dir);
                    print!("{}", retro::run_all(&root, 5, include_tmp)?);
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
        } => {
            daemon::run(session_dir, parent_pid, mode.into(), &agent)?;
            Ok(0)
        }
        Cmd::Hook { .. } | Cmd::Run { .. } => unreachable!("handled in main"),
    }
}
