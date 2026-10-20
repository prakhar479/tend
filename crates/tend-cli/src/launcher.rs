//! `tend claude`: start the daemon, then run Claude Code with tend hooks.
//! If anything of tend's own fails (setup, daemon), run Claude Code without tend
//! (fail open, rule 1).

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tend_core::Mode;

use crate::connector::claude_code;
use crate::ipc::{self, Request};
use crate::paths;

/// What `tend claude` set up for a session.
struct Session {
    id: String,
    dir: PathBuf,
    sock: PathBuf,
    token: String,
    settings: PathBuf,
    daemon: Child,
}

pub fn run_claude(mode: Mode, args: Vec<String>) -> Result<i32> {
    let (mode, args) = take_mode(mode, args);
    let bin = env::var("TEND_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    // A missing agent is the user's to fix; set nothing up for it.
    if find_program(&bin).is_none() {
        bail!("`{bin}` not found on PATH (set TEND_CLAUDE_BIN to use another binary)");
    }
    let (user_settings, args) = take_settings(args);
    let exe = env::current_exe().context("find tend executable")?;

    let session = match setup(&exe, mode, user_settings.as_deref()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("tend: {e:#}; running {bin} without tend");
            None
        }
    };

    forward_signals();
    let mut cmd = Command::new(&bin);
    match &session {
        Some(s) => {
            cmd.arg("--settings")
                .arg(&s.settings)
                .env("TEND_SOCKET", &s.sock)
                .env("TEND_SESSION_DIR", &s.dir)
                .env("PATH", agent_path(&exe, &s.dir));
        }
        // tend could not take the user's --settings over: pass it on unchanged.
        None => {
            if let Some(v) = &user_settings {
                cmd.arg("--settings").arg(v);
            }
        }
    }
    let status = run_child(cmd.args(&args)).with_context(|| format!("start `{bin}`"))?;

    if let Some(mut s) = session {
        finish(&mut s);
    }
    Ok(exit_code(status))
}

/// Session dir, token, settings, daemon. Any error here means: run without tend.
fn setup(exe: &Path, mode: Mode, user_settings: Option<&str>) -> Result<Session> {
    let root = paths::sessions_dir();
    // A crashed daemon leaves its socket behind. Clean up before starting a new one.
    let _ = crate::sessions::remove_stale_sockets(&root);
    paths::ensure_private_dir(&paths::home())?;
    let keep_days = crate::config::load().keep_days;
    if keep_days > 0 {
        let root = root.clone();
        // Old sessions hold copies of tool output; drop them without delaying the start.
        thread::spawn(move || {
            let now = paths::now_ms() / 1000;
            let _ = crate::sessions::prune(&root, keep_days, now);
        });
    }
    let id = paths::new_session_id();
    let dir = root.join(&id);
    paths::ensure_private_dir(&dir)?;
    let token = paths::new_token(&dir)?;
    let mut settings = claude_code::settings(exe);
    if let Some(user) = user_settings {
        merge_settings(&mut settings, &read_settings(user)?);
    }
    let settings_path = dir.join("claude-settings.json");
    fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    let sock = paths::socket_path(&dir);
    let mut daemon = start_daemon(exe, &dir, mode)?;
    if !wait_for(&sock) {
        let _ = daemon.kill();
        let _ = daemon.wait();
        bail!(
            "daemon did not start (see {})",
            dir.join("daemon.log").display()
        );
    }
    Ok(Session {
        id,
        dir,
        sock,
        token,
        settings: settings_path,
        daemon,
    })
}

/// Stop the daemon and print the one-line summary. Say so when the daemon died early:
/// a quiet session and a dead supervisor must not look the same.
fn finish(s: &mut Session) {
    let died = match s.daemon.try_wait() {
        Ok(Some(status)) => Some(status),
        _ => None,
    };
    if died.is_none() {
        let req = Request::Shutdown {
            token: Some(s.token.clone()),
        };
        let _ = ipc::send(&s.sock, &req, Duration::from_millis(500));
        // Reap it, so no zombie is left; give up after a short wait.
        for _ in 0..50 {
            if matches!(s.daemon.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    let entries = crate::store::read_entries(&s.dir).unwrap_or_default();
    // `claude --version`, `claude mcp …`: no conversation started, nothing to keep.
    let events = entries
        .iter()
        .filter(|e| matches!(e.body, tend_core::Body::Event { .. }))
        .count();
    if events == 0 && died.is_none() {
        let _ = fs::remove_dir_all(&s.dir);
        return;
    }
    let summary = crate::report::one_line(&entries);
    eprintln!(
        "tend: session {} ended · {summary} · `tend report` for details",
        s.id
    );
    if let Some(status) = died {
        let when = entries
            .last()
            .map(|e| format!(" after entry {}", e.seq))
            .unwrap_or_default();
        eprintln!(
            "tend: the daemon stopped during the session{when} ({status}); later tool calls were not recorded (see {})",
            s.dir.join("daemon.log").display()
        );
    }
}

/// `--tend-mode X` or `--tend-mode=X` anywhere in the arguments (clap only sees it
/// before the first argument for claude).
fn take_mode(mut mode: Mode, args: Vec<String>) -> (Mode, Vec<String>) {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let value = if a == "--tend-mode" {
            it.next()
        } else if let Some(v) = a.strip_prefix("--tend-mode=") {
            Some(v.to_string())
        } else {
            out.push(a);
            continue;
        };
        match value.as_deref() {
            Some("look") => mode = Mode::Look,
            Some("optimize") => mode = Mode::Optimize,
            other => eprintln!(
                "tend: --tend-mode takes look or optimize, got {}; using {}",
                other.unwrap_or("nothing"),
                mode_name(mode)
            ),
        }
    }
    (mode, out)
}

/// The user's own `--settings` value. Claude Code may keep only one `--settings`, so tend
/// merges the user's into its own file instead of passing both.
fn take_settings(args: Vec<String>) -> (Option<String>, Vec<String>) {
    let mut user = None;
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--settings" {
            user = it.next();
        } else if let Some(v) = a.strip_prefix("--settings=") {
            user = Some(v.to_string());
        } else {
            out.push(a);
        }
    }
    (user, out)
}

/// `--settings` takes a file or a JSON string.
fn read_settings(value: &str) -> Result<Value> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        fs::read_to_string(value).with_context(|| format!("read --settings {value}"))?
    };
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse --settings {value}"))?;
    if !v.is_object() {
        bail!("--settings {value} is not a JSON object");
    }
    Ok(v)
}

/// Put the user's settings under tend's: the user's keys win, except that hooks are
/// combined, the user's first, per event.
fn merge_settings(tend: &mut Value, user: &Value) {
    let tend_hooks = tend.get("hooks").cloned().unwrap_or(Value::Null);
    let mut merged = user.clone();
    let mut hooks = user
        .get("hooks")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let (Some(h), Some(ours)) = (hooks.as_object_mut(), tend_hooks.as_object()) {
        for (event, entries) in ours {
            let list = h
                .entry(event.clone())
                .or_insert_with(|| serde_json::json!([]));
            if let (Some(list), Some(entries)) = (list.as_array_mut(), entries.as_array()) {
                list.extend(entries.iter().cloned());
            }
        }
    }
    merged["hooks"] = hooks;
    *tend = merged;
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Look => "look",
        Mode::Optimize => "optimize",
    }
}

fn start_daemon(exe: &Path, dir: &Path, mode: Mode) -> Result<Child> {
    let log = File::create(dir.join("daemon.log"))?;
    Command::new(exe)
        .arg("daemon")
        .arg("--session-dir")
        .arg(dir)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--mode")
        .arg(mode_name(mode))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Own process group: Ctrl-C in the terminal must not reach the daemon.
        .process_group(0)
        .spawn()
        .context("spawn daemon")
}

fn wait_for(sock: &Path) -> bool {
    for _ in 0..1000 {
        if ipc::send(sock, &Request::Ping, Duration::from_millis(200)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

/// The agent's pid while it runs (0 before), for the signal handler.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    // Claude Code handles Ctrl-C itself: the terminal sends it to the whole group.
    if pid > 0 && sig != libc::SIGINT {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The launcher must outlive the agent to shut the daemon down. SIGINT is the agent's
/// (it gets it from the terminal); SIGTERM, SIGHUP, and SIGQUIT go to the agent, so
/// `kill <launcher>` stops claude too instead of leaving it unsupervised. A handler (not
/// SIG_IGN) resets to the default in the child on exec.
fn forward_signals() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: installs an async-signal-safe handler.
        unsafe {
            libc::signal(
                sig,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

fn run_child(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    let status = child.wait();
    CHILD.store(0, Ordering::SeqCst);
    status
}

/// The agent's exit code; 128 + signal when a signal ended it, as a shell reports it.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// The first `name` on PATH (or `name` itself when it has a slash).
fn find_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The agent's PATH: a private `<session>/bin` with a `tend` link first, so the agent can
/// run `tend show` without the rest of tend's install directory (say `~/.cargo/bin` or
/// `target/release`) shadowing its tools. Falls back to tend's own directory.
fn agent_path(exe: &Path, dir: &Path) -> OsString {
    let bin = dir.join("bin");
    let linked = fs::create_dir_all(&bin).is_ok()
        && std::os::unix::fs::symlink(exe, bin.join("tend")).is_ok();
    let first = if linked {
        Some(bin)
    } else {
        exe.parent().map(Path::to_path_buf)
    };
    let mut parts: Vec<PathBuf> = first.into_iter().collect();
    if let Some(p) = env::var_os("PATH") {
        parts.extend(env::split_paths(&p));
    }
    env::join_paths(parts).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tend_mode_is_found_anywhere() {
        let (m, rest) = take_mode(
            Mode::Look,
            strings(&["-p", "hi", "--tend-mode", "optimize"]),
        );
        assert!(matches!(m, Mode::Optimize));
        assert_eq!(rest, strings(&["-p", "hi"]));
        let (m, rest) = take_mode(Mode::Optimize, strings(&["--tend-mode=look", "--resume"]));
        assert!(matches!(m, Mode::Look));
        assert_eq!(rest, strings(&["--resume"]));
    }

    #[test]
    fn user_settings_are_merged_under_tend_hooks() {
        let (user, rest) = take_settings(strings(&["--settings", r#"{"model":"x"}"#, "-p", "hi"]));
        assert_eq!(rest, strings(&["-p", "hi"]));
        let user = read_settings(&user.unwrap()).unwrap();
        let mut tend = json!({"hooks": {"Stop": [{"hooks": [{"command": "tend hook Stop"}]}]}});
        let user = json!({
            "model": user["model"],
            "hooks": {"Stop": [{"hooks": [{"command": "mine"}]}]}
        });
        merge_settings(&mut tend, &user);
        assert_eq!(tend["model"], "x");
        let stop = tend["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "mine");
        assert_eq!(stop[1]["hooks"][0]["command"], "tend hook Stop");
        assert!(read_settings("[1]").is_err());
    }
}
