//! Session housekeeping: prune old sessions, remove stale sockets.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use crate::paths::socket_path;

/// A daemon listens on this socket. Only "refused" or "not found" mean stale: a busy
/// daemon may answer slowly, and its socket must survive (R0.3).
pub fn is_live(dir: &Path) -> bool {
    match UnixStream::connect(socket_path(dir)) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
    }
}

fn dirs(root: &Path) -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    d.sort();
    d
}

/// Remove sockets that no daemon answers on (a crashed or killed daemon leaves them).
/// Returns the sockets removed.
pub fn remove_stale_sockets(root: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for dir in dirs(root) {
        let sock = socket_path(&dir);
        if sock.exists() && !is_live(&dir) && fs::remove_file(&sock).is_ok() {
            removed.push(sock);
        }
    }
    removed
}

/// Last activity in seconds: the newest of the id's start time and the ledger's mtime.
fn last_active_secs(dir: &Path) -> u64 {
    let started = dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('-').next())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let modified = fs::metadata(dir.join("ledger.jsonl"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    started.max(modified)
}

/// Delete sessions with no activity for `days` days. Live sessions are kept.
/// Returns the directories removed.
pub fn prune(root: &Path, days: u64, now_secs: u64) -> Result<Vec<PathBuf>> {
    let cutoff = now_secs.saturating_sub(days * 86_400);
    let mut removed = Vec::new();
    for dir in dirs(root) {
        if last_active_secs(&dir) >= cutoff || is_live(&dir) {
            continue;
        }
        let _ = fs::remove_file(socket_path(&dir));
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        removed.push(dir);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn root(name: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("tend-sessions-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(&r).unwrap();
        r
    }

    #[test]
    fn prune_removes_only_old_dead_sessions() {
        let r = root("prune");
        let now = 2_000_000_000;
        let old = r.join(format!("{:010}-1", now - 10 * 86_400));
        let recent = r.join(format!("{:010}-2", now - 86_400));
        for d in [&old, &recent] {
            fs::create_dir_all(d).unwrap();
        }
        let removed = prune(&r, 7, now).unwrap();
        assert_eq!(removed, vec![old.clone()]);
        assert!(!old.exists() && recent.exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn stale_socket_is_removed_live_one_is_kept() {
        let r = root("sock");
        let dead = r.join("0000000001-1");
        let live = r.join("0000000002-2");
        fs::create_dir_all(&dead).unwrap();
        fs::create_dir_all(&live).unwrap();
        // A socket file nobody listens on.
        drop(UnixListener::bind(socket_path(&dead)).unwrap());
        // A listener that answers pings, like a daemon.
        let l = UnixListener::bind(socket_path(&live)).unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            for s in l.incoming().flatten() {
                let mut line = String::new();
                let _ = BufReader::new(&s).read_line(&mut line);
                let _ = writeln!(&s, "{{\"message\":\"pong\"}}");
            }
        });
        assert_eq!(remove_stale_sockets(&r), vec![socket_path(&dead)]);
        assert!(socket_path(&live).exists());
        // An old but live session is not pruned either.
        assert!(prune(&r, 0, u64::MAX / 2)
            .unwrap()
            .iter()
            .all(|d| d != &live));
        let _ = fs::remove_dir_all(&r);
    }
}
