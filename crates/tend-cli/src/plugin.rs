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

/// A conversation id as a file name: only letters, digits, and `-`, at most 64.
fn clean_id(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

/// The session directory a conversation id maps to; a new one on its first hook.
pub fn session_dir(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let id = clean_id(session_id);
    if id.is_empty() {
        return None;
    }
    let map_dir = paths::home().join("by-session");
    let map = map_dir.join(&id);
    if let Some(dir) = mapped_dir(&paths::sessions_dir(), &map) {
        return Some(dir);
    }
    paths::ensure_private_dir(&paths::home()).ok()?;
    paths::ensure_private_dir(&map_dir).ok()?;
    let secs = paths::now_ms() / 1000;
    let name = format!("{secs:010}-p{}", &id[..id.len().min(8)]);
    let dir = paths::sessions_dir().join(&name);
    paths::ensure_private_dir(&dir).ok()?;
    paths::new_token(&dir).ok()?;
    // Publish the mapping all at once, and only if nobody did first: hooks of one
    // conversation can run in parallel, and each must end up with the same daemon.
    let tmp = map_dir.join(format!(".{id}.{}", std::process::id()));
    fs::write(&tmp, &name).ok()?;
    let linked = fs::hard_link(&tmp, &map);
    let _ = fs::remove_file(&tmp);
    if linked.is_err() {
        if let Some(winner) = mapped_dir(&paths::sessions_dir(), &map) {
            if winner != dir {
                let _ = fs::remove_dir_all(&dir);
            }
            return Some(winner);
        }
        return None;
    }
    let cfg = config::load();
    if cfg.experimental_audit {
        launcher::record_audit_base(&dir, cwd);
    }
    Some(dir)
}

/// The session directory for a status line's stdin JSON (`session_id`), if the plugin
/// started one.
pub fn existing_session_dir(session_id: &str) -> Option<PathBuf> {
    // An id is a name in `by-session/`, never a path; clean it as `session_dir` did.
    let id = clean_id(session_id);
    if id.is_empty() {
        return None;
    }
    mapped_dir(
        &paths::sessions_dir(),
        &paths::home().join("by-session").join(id),
    )
}

/// The session directory a mapping file names. An empty or path-like name (a file
/// caught half written, or damaged) names nothing: it must not resolve to `sessions/`.
fn mapped_dir(sessions: &Path, map: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(map).ok()?;
    let name = text.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let dir = sessions.join(name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_odd_mapping_names_no_session() {
        let root = std::env::temp_dir().join(format!("tend-plugin-map-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sessions/s1")).unwrap();
        let map = root.join("map");
        for bad in ["", "\n", "..", "../x", "a/b"] {
            fs::write(&map, bad).unwrap();
            assert_eq!(mapped_dir(&root.join("sessions"), &map), None, "{bad:?}");
        }
        fs::write(&map, "s1\n").unwrap();
        assert_eq!(
            mapped_dir(&root.join("sessions"), &map),
            Some(root.join("sessions/s1"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn both_lookups_clean_the_id_alike() {
        assert_eq!(clean_id("ab_cd:ef-1"), "abcdef-1");
        assert_eq!(clean_id(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn a_session_id_is_never_a_path() {
        for id in ["", "../x", "a/b", ".hidden"] {
            assert!(existing_session_dir(id).is_none(), "{id}");
        }
    }
}
