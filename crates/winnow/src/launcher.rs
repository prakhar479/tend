//! `winnow claude`: start the daemon, then run Claude Code with winnow hooks.
//! If the daemon fails, run Claude Code without winnow (fail open).

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use winnow_core::Mode;

use crate::connector::claude_code;
use crate::ipc::{self, Request};
use crate::paths;

pub fn run_claude(mode: Mode, args: Vec<String>) -> Result<i32> {
    let exe = env::current_exe().context("find winnow executable")?;
    // A crashed daemon leaves its socket behind. Clean up before starting a new one.
    let _ = crate::sessions::remove_stale_sockets(&paths::sessions_dir());
    let id = paths::new_session_id();
    let dir = paths::sessions_dir().join(&id);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let settings_path = dir.join("claude-settings.json");
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&claude_code::settings(&exe))?,
    )?;
    let sock = paths::socket_path(&dir);

    let daemon_ok = match start_daemon(&exe, &dir, mode) {
        Ok(()) => wait_for(&sock),
        Err(e) => {
            eprintln!("winnow: {e:#}");
            false
        }
    };
    if !daemon_ok {
        eprintln!(
            "winnow: daemon did not start; running claude without winnow (see {})",
            dir.join("daemon.log").display()
        );
    }

    ignore_sigint();
    let bin = env::var("WINNOW_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut cmd = Command::new(&bin);
    if daemon_ok {
        cmd.arg("--settings")
            .arg(&settings_path)
            .env("WINNOW_SOCKET", &sock)
            .env("WINNOW_SESSION_DIR", &dir)
            .env("PATH", path_with_exe_dir(&exe));
    }
    let status = cmd
        .args(&args)
        .status()
        .with_context(|| format!("start `{bin}`"))?;

    if daemon_ok {
        let _ = ipc::send(&sock, &Request::Shutdown, Duration::from_millis(500));
        eprintln!("winnow: session {id} ended · run `winnow report` for details");
    }
    Ok(status.code().unwrap_or(1))
}

fn start_daemon(exe: &Path, dir: &Path, mode: Mode) -> Result<()> {
    let log = File::create(dir.join("daemon.log"))?;
    let mode = match mode {
        Mode::Look => "look",
        Mode::Optimize => "optimize",
    };
    Command::new(exe)
        .arg("daemon")
        .arg("--session-dir")
        .arg(dir)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--mode")
        .arg(mode)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Own process group: Ctrl-C in the terminal must not reach the daemon.
        .process_group(0)
        .spawn()
        .context("spawn daemon")?;
    Ok(())
}

fn wait_for(sock: &Path) -> bool {
    for _ in 0..100 {
        if ipc::send(sock, &Request::Ping, Duration::from_millis(200)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

extern "C" fn on_sigint(_: libc::c_int) {}

/// Claude Code uses Ctrl-C itself. The launcher must survive it to shut the daemon down.
/// A handler (not SIG_IGN) resets to default in the child on exec.
fn ignore_sigint() {
    // SAFETY: installs an async-signal-safe no-op handler.
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

/// Put winnow's directory first on PATH so the agent can run `winnow show`.
fn path_with_exe_dir(exe: &Path) -> OsString {
    let mut parts: Vec<PathBuf> = exe
        .parent()
        .map(|p| vec![p.to_path_buf()])
        .unwrap_or_default();
    if let Some(p) = env::var_os("PATH") {
        parts.extend(env::split_paths(&p));
    }
    env::join_paths(parts).unwrap_or_default()
}
