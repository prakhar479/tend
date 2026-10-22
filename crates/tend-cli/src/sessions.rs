//! Session housekeeping: prune old sessions, remove stale sockets.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};

use crate::paths::{self, socket_path};

/// A daemon listens on this socket. Only "refused" or "not found" mean stale: a busy
/// daemon may answer slowly, and its socket must survive (R0.3).
pub fn is_live(dir: &Path) -> bool {
    match crate::ipc::connect_now(&socket_path(dir)) {
        Ok(_) => true,
        // "Refused" and "not found" mean no daemon; so does a path the OS cannot use.
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::InvalidInput
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
    // The dir's own mtime covers a session with no ledger and an id that is not a
    // timestamp: neither may count as "last active in 1970".
    let touched = fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    started.max(modified).max(touched)
}

/// Delete sessions with no activity for `days` days. Live sessions are kept.
/// Returns the directories removed.
pub fn prune(root: &Path, days: u64, now_secs: u64) -> Result<Vec<PathBuf>> {
    let cutoff = now_secs.saturating_sub(days.saturating_mul(86_400));
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

/// The project a session ran in: `cwd` of its `session_started` event (the first lines).
pub fn session_cwd(dir: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(dir.join("ledger.jsonl")).ok()?;
    BufReader::new(f)
        .lines()
        .take(4)
        .map_while(Result::ok)
        .find_map(|l| {
            let v: serde_json::Value = serde_json::from_str(&l).ok()?;
            let ev = v.get("body")?.get("event")?;
            (ev.get("type")?.as_str()? == "session_started")
                .then(|| ev.get("cwd")?.as_str().map(str::to_string))
                .flatten()
        })
}

/// Which session a command means (I4, U1, U3, U5).
/// - `spec`: a path, an id or unique id prefix (as `tend sessions` prints), or `-N` for
///   the Nth newest.
/// - No `spec`: the session this shell runs in (`TEND_SESSION_DIR`), else the newest one
///   started in the current directory, else the newest.
/// - `live`: only sessions whose daemon runs (for `tend mode` and `tend allow`).
pub fn resolve(spec: Option<&str>, live: bool) -> Result<PathBuf> {
    let root = paths::sessions_dir();
    let mut all = dirs(&root);
    all.reverse(); // newest first: ids start with the start time
    let name = |d: &Path| {
        d.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    if let Some(spec) = spec {
        if spec.is_empty() {
            bail!("empty session name (see `tend sessions`)");
        }
        let dir = if Path::new(spec).is_dir() {
            PathBuf::from(spec)
        } else if let Some(n) = spec.strip_prefix('-').and_then(|n| n.parse::<usize>().ok()) {
            all.get(n.max(1) - 1)
                .cloned()
                .ok_or_else(|| anyhow!("there are only {} sessions", all.len()))?
        } else {
            let mut hits: Vec<&PathBuf> =
                all.iter().filter(|d| name(d).starts_with(spec)).collect();
            // An exact id wins over longer ids it is a prefix of.
            if let Some(exact) = hits.iter().find(|d| name(d) == spec) {
                hits = vec![*exact];
            }
            match hits[..] {
                [one] => one.clone(),
                [] => bail!("no session `{spec}` (see `tend sessions`)"),
                _ => bail!(
                    "`{spec}` matches {} sessions: {}",
                    hits.len(),
                    hits.iter()
                        .take(5)
                        .map(|d| name(d))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        };
        if live && !is_live(&dir) {
            bail!("session {} is not live", name(&dir));
        }
        return Ok(dir);
    }
    if let Some(p) = std::env::var_os("TEND_SESSION_DIR") {
        return Ok(PathBuf::from(p));
    }
    let here = std::env::current_dir()
        .ok()
        .map(|d| d.to_string_lossy().to_string());
    let in_here = |d: &&PathBuf| here.is_some() && session_cwd(d) == here;
    if live {
        let running: Vec<PathBuf> = all.into_iter().filter(|d| is_live(d)).collect();
        let local: Vec<&PathBuf> = running.iter().filter(in_here).collect();
        let pick: Vec<&PathBuf> = if local.is_empty() {
            running.iter().collect()
        } else {
            local
        };
        return match pick[..] {
            [one] => Ok(one.clone()),
            [] => bail!("no live tend session (start one with `tend claude`)"),
            _ => bail!(
                "{} live sessions; choose one with --session: {}",
                pick.len(),
                pick.iter()
                    .map(|d| format!("{} ({})", name(d), session_cwd(d).unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    all.iter()
        .find(in_here)
        .or(all.first())
        .cloned()
        .ok_or_else(|| anyhow!("no sessions in {}", root.display()))
}

/// One line per session for `tend sessions`, newest first.
pub fn list(root: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut all = dirs(root);
    all.reverse();
    for dir in all {
        let entries = crate::store::read_entries(&dir).unwrap_or_default();
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let start = id
            .split('-')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let first = entries.iter().map(|e| e.ts_ms).filter(|t| *t > 0).min();
        let last = entries.iter().map(|e| e.ts_ms).max();
        let dur = match (first, last) {
            (Some(a), Some(b)) if b >= a => crate::report::minutes(b - a),
            _ => "-".into(),
        };
        let (mut mode, mut tools) = (String::from("?"), 0u64);
        for e in &entries {
            match &e.body {
                tend_core::Body::Event {
                    event: tend_core::Event::SessionStarted { mode: Some(m), .. },
                } => mode = format!("{m:?}").to_lowercase(),
                tend_core::Body::Event {
                    event: tend_core::Event::ToolCompleted { .. },
                } => tools += 1,
                _ => {}
            }
        }
        let flags = crate::store::read_flags(&dir).len();
        let _ = writeln!(
            out,
            "{id:<18} {}  {dur:>8}  {mode:<8} {tools:>4} calls{}{}  {}",
            utc(start),
            if flags > 0 {
                format!(" · {flags} flags")
            } else {
                String::new()
            },
            if is_live(&dir) { " · live" } else { "" },
            session_cwd(&dir).unwrap_or_default(),
        );
    }
    out
}

/// `YYYY-MM-DD HH:MM` (UTC) for unix seconds.
fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
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

    /// `--older-than` with a huge number of days must not wrap and purge everything.
    #[test]
    fn prune_with_huge_days_keeps_everything() {
        let r = root("huge");
        let now = 2_000_000_000;
        let recent = r.join(format!("{:010}-2", now - 86_400));
        fs::create_dir_all(&recent).unwrap();
        let days = u64::MAX / 86_400 + 2;
        assert!(prune(&r, days, now).unwrap().is_empty());
        assert!(recent.exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn stale_socket_is_removed_live_one_is_kept() {
        let r = root("sock");
        let dead = r.join("0000000001-1");
        let live = r.join("0000000002-2");
        fs::create_dir_all(&dead).unwrap();
        fs::create_dir_all(&live).unwrap();
        // A long temp dir (macOS) puts sockets in the shared fallback dir; the daemon
        // creates it, so the test must too.
        for d in [&dead, &live] {
            fs::create_dir_all(socket_path(d).parent().unwrap()).unwrap();
        }
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

    /// A session dir whose id is not a timestamp and that has no ledger yet is new, not old.
    #[test]
    fn prune_keeps_a_fresh_dir_with_an_odd_name() {
        let r = root("odd");
        let d = r.join("agent-1");
        fs::create_dir_all(&d).unwrap();
        let now = UNIX_EPOCH.elapsed().unwrap().as_secs();
        assert!(prune(&r, 7, now).unwrap().is_empty());
        assert!(d.exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(1_792_398_944), "2026-10-19 08:35");
        assert_eq!(utc(1_709_251_199), "2024-02-29 23:59");
    }
}
