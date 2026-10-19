//! Where rein keeps session data.
//!
//! Layout: <home>/sessions/<id>/
//!   ledger.jsonl        append-only ledger
//!   blobs/<sha256>      original outputs and edit texts
//!   hooks.jsonl         raw hook payloads, only with REIN_CAPTURE=1 (they hold code)
//!   claude-settings.json, daemon.log, reind.sock (in <tmp>/rein-<uid>/ if the path is long)
//!
//! Directories are private (0700): sessions hold code, outputs, and a control socket.

use std::env;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
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
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    env::temp_dir()
        .join(format!("rein-{uid}"))
        .join(format!("{}.sock", &hash[..16]))
}

/// Create `dir` (and missing parents) as 0700. An existing dir must belong to us;
/// a shared temp dir could hold one that another user made.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let meta = fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::getuid() } {
        bail!("{} belongs to another user", dir.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("make {} private", dir.display()))?;
    }
    Ok(())
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
        let uid = unsafe { libc::getuid() };
        assert!(sock.parent().unwrap().ends_with(format!("rein-{uid}")));
    }

    #[test]
    fn private_dirs() {
        let base = env::temp_dir().join(format!("rein-private-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let d = base.join("a/b");
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        let mode = fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "tightened");
        let _ = fs::remove_dir_all(&base);
    }
}
