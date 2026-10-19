//! rein CLI: launcher, hook shim, session daemon, and inspection commands.

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
use rein_core::Mode;

#[derive(Parser)]
#[command(
    name = "rein",
    version,
    about = "Local supervisor for command-line coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start Claude Code under rein. Arguments after the options go to `claude`.
    Claude {
        #[arg(long = "rein-mode", value_enum, default_value = "look")]
        mode: ModeArg,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start Codex under rein (Phase 2).
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install stable hooks for an agent once (Phase 2: codex).
    Init { agent: String },
    /// Change the mode of a live session.
    Mode {
        #[arg(value_enum)]
        mode: ModeArg,
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Allow one edit the test guard blocked (the latest, or action a-N). Run it yourself,
    /// for example with `! rein allow` in Claude Code; the agent may not.
    Allow {
        id: Option<String>,
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Show what happened in a session.
    Report {
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Print the full original of a summarized output (o-N).
    Show {
        id: String,
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Show the cause chain of an action (a-N), output (o-N), or entry number.
    Why {
        id: String,
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Reverse an action (Phase 3).
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
    },
    /// List recorded sessions.
    Sessions {
        /// Delete sessions with no activity for this many days (live sessions are kept).
        #[arg(long, value_name = "DAYS")]
        prune: Option<u64>,
    },
    /// Wrapper rein puts around a test command (see run.rs). Keeps the exit code.
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
    let cli = Cli::parse();
    // The shim must never fail the agent: handle it before anything that can error.
    if let Cmd::Hook { event } = &cli.cmd {
        shim::run(event);
        std::process::exit(0);
    }
    if let Cmd::Run { id, sh } = &cli.cmd {
        std::process::exit(run::run(id, sh));
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("rein: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Claude { mode, args } => launcher::run_claude(mode.into(), args),
        Cmd::Codex { .. } => bail!("the Codex connector is Phase 2 (see docs/TASKS.md)"),
        Cmd::Init { agent } => bail!("`rein init {agent}` is Phase 2 (see docs/TASKS.md)"),
        Cmd::Undo { .. } => bail!("undo is Phase 3 (see docs/TASKS.md)"),
        Cmd::Mode { mode, session } => {
            let dir = paths::resolve_session(session)?;
            let req = ipc::Request::SetMode { mode: mode.into() };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))?;
            println!("{}", reply.message.unwrap_or_default());
            Ok(0)
        }
        Cmd::Allow { id, session } => {
            let dir = paths::resolve_session(session)?;
            let req = ipc::Request::Allow { action: id };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
                .with_context(|| format!("session {} is not live", dir.display()))?;
            println!("{}", reply.message.unwrap_or_default());
            Ok(0)
        }
        Cmd::Report { session } => {
            let dir = paths::resolve_session(session)?;
            let entries = store::read_entries(&dir)?;
            let label = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            print!("{}", report::render(&label, &entries));
            Ok(0)
        }
        Cmd::Show { id, session } => {
            let dir = paths::resolve_session(session)?;
            let entries = store::read_entries(&dir)?;
            let hash = report::find_output_blob(&entries, &id)
                .ok_or_else(|| anyhow::anyhow!("no output {id} in {}", dir.display()))?;
            print!("{}", store::read_blob(&dir, &hash)?);
            Ok(0)
        }
        Cmd::Why { id, session } => {
            let dir = paths::resolve_session(session)?;
            let entries = store::read_entries(&dir)?;
            print!("{}", report::why(&entries, &id)?);
            Ok(0)
        }
        Cmd::Retro {
            transcript,
            all,
            projects,
        } => {
            match transcript {
                Some(t) if !all => print!("{}", retro::run(&t)?),
                _ => {
                    let root = projects.unwrap_or_else(paths::claude_projects_dir);
                    print!("{}", retro::run_all(&root, 5)?);
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
            for dir in paths::list_sessions() {
                let live = sessions::is_live(&dir);
                let n = store::read_entries(&dir).map(|e| e.len()).unwrap_or(0);
                println!(
                    "{}  {:>6} entries{}",
                    dir.display(),
                    n,
                    if live { "  (live)" } else { "" }
                );
            }
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
