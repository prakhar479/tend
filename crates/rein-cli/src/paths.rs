//! Where rein keeps session data.
//!
//! Layout: <home>/sessions/<id>/
//!   ledger.jsonl        append-only ledger
//!   blobs/<sha256>      original outputs and edit texts
//!   hooks.jsonl         raw hook payloads (set REIN_CAPTURE=0 to disable)
//!   claude-settings.json, daemon.log, reind.sock (in the temp dir if the path is long)

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rein_core::text::sha256_hex;

pub fn home() -> PathBuf {
    if let Some(h) = env::var_os("REIN_HOME") {
        return PathBuf::from(h);
    }
    if let Some(x) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("rein");
    }
    let base = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    base.join(".local/share/rein")
}

/// Where Claude Code keeps session transcripts.
pub fn claude_projects_dir() -> PathBuf {
    if let Some(d) = env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d).join("projects");
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".claude/projects")
}

pub fn sessions_dir() -> PathBuf {
    home().join("sessions")
}

/// Longest socket path we use. Unix socket paths are limited to about 108 bytes.
const MAX_SOCKET_PATH: usize = 100;

/// The session socket. A long session path falls back to the temp dir, named by a hash
/// of the session path, so the daemon and the CLI find the same socket.
pub fn socket_path(dir: &Path) -> PathBuf {
    let p = dir.join("reind.sock");
    if p.as_os_str().len() <= MAX_SOCKET_PATH {
        return p;
    }
    let hash = sha256_hex(&dir.to_string_lossy());
    env::temp_dir().join(format!("rein-{}.sock", &hash[..16]))
}

/// Sortable session id: zero-padded unix seconds plus pid.
pub fn new_session_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:010}-{}", std::process::id())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn list_sessions() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(sessions_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// Explicit path, then $REIN_SESSION_DIR (set inside a rein session), then the latest session.
pub fn resolve_session(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(p) = env::var_os("REIN_SESSION_DIR") {
        return Ok(PathBuf::from(p));
    }
    list_sessions()
        .pop()
        .ok_or_else(|| anyhow!("no sessions in {}", sessions_dir().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_session_paths_get_a_short_socket() {
        let short = Path::new("/home/dev/.local/share/rein/sessions/1-2");
        assert_eq!(socket_path(short), short.join("reind.sock"));
        let long = PathBuf::from(format!("/home/dev/{}/sessions/1-2", "x".repeat(120)));
        let sock = socket_path(&long);
        assert!(
            sock.as_os_str().len() <= MAX_SOCKET_PATH,
            "{}",
            sock.display()
        );
        assert_eq!(sock, socket_path(&long), "same path both sides");
    }
}
