//! Plugin mode: tend's hooks installed as a Claude Code plugin, so the user keeps
//! typing `claude` (I6 E6). There is no launcher, so the first hook of a conversation
//! starts its daemon, found again by Claude Code's `session_id`:
//! `<home>/by-session/<session_id>` holds the session's directory name.
//!
//! Rules, as for the shim: exit 0, print nothing on any error, never block the agent.
//! When the launcher runs the session (`TEND_SOCKET` is set), its own hooks handle it,
//! and plugin hooks do nothing, so nothing is counted twice.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::ipc::{self, Request};
use crate::{config, launcher, paths, shim};

/// A plugin daemon exits after this long without a hook (a crashed or killed agent).
const IDLE_EXIT_MINUTES: u64 = 120;

pub fn run(event: &str) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if std::env::var_os("TEND_SOCKET").is_some() {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    let Some(session_id) = payload.get("session_id").and_then(Value::as_str) else {
        return;
    };
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let Some(dir) = session_dir(session_id, Path::new(cwd)) else {
        return;
    };
    let sock = paths::socket_path(&dir);
    if event == "SessionEnd" && !alive(&sock) {
        return;
    }
    if !alive(&sock) && !start(&dir, &sock) {
        return;
    }
    let timeout = std::env::var("TEND_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    if let Some(out) = shim::forward(Some(&sock), event, &input, Duration::from_millis(timeout)) {
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "{out}");
    }
    if event == "SessionEnd" {
        let _ = launcher::finish_audit(&dir);
        let req = Request::Shutdown {
            token: paths::read_token(&dir),
        };
        let _ = ipc::send(&sock, &req, Duration::from_millis(500));
    }
}

/// The session directory a conversation id maps to; a new one on its first hook.
pub fn session_dir(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let id: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect();
    if id.is_empty() {
        return None;
    }
    let map_dir = paths::home().join("by-session");
    let map = map_dir.join(&id);
    if let Ok(name) = fs::read_to_string(&map) {
        let dir = paths::sessions_dir().join(name.trim());
        if dir.is_dir() {
            return Some(dir);
        }
    }
    paths::ensure_private_dir(&paths::home()).ok()?;
    paths::ensure_private_dir(&map_dir).ok()?;
    let secs = paths::now_ms() / 1000;
    let name = format!("{secs:010}-p{}", &id[..id.len().min(8)]);
    let dir = paths::sessions_dir().join(&name);
    paths::ensure_private_dir(&dir).ok()?;
    paths::new_token(&dir).ok()?;
    fs::write(&map, &name).ok()?;
    let cfg = config::load();
    if cfg.experimental_audit {
        launcher::record_audit_base(&dir, cwd);
    }
    Some(dir)
}

/// The session directory for a status line's stdin JSON (`session_id`), if the plugin
/// started one.
pub fn existing_session_dir(session_id: &str) -> Option<PathBuf> {
    let name = fs::read_to_string(paths::home().join("by-session").join(session_id)).ok()?;
    let dir = paths::sessions_dir().join(name.trim());
    dir.is_dir().then_some(dir)
}

fn alive(sock: &Path) -> bool {
    ipc::send(sock, &Request::Ping, Duration::from_millis(100)).is_ok()
}

/// Start this session's daemon in the configured plugin mode; true once it answers.
fn start(dir: &Path, sock: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mode = config::load().plugin_mode;
    launcher::start_daemon(&exe, dir, mode, None, Some(IDLE_EXIT_MINUTES)).is_ok()
        && launcher::wait_for(sock)
}
