//! Fleet awareness (F1), the I/O side: find the other live tend sessions on this
//! machine, tail their ledgers, and turn what matters to this session into
//! `Event::Peer` for its controller. Fails open: an error skips a peer for one round.
//!
//! No hub process: each daemon reads its peers' ledgers. The hub comes with spawning
//! (F3), where something must own the sessions.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use tend_core::{ActionKind, Body, Entry, Event, PeerActivity};

use crate::paths::{self, socket_path};
use crate::sessions::is_live;

/// Read at most this much of one peer's ledger per round; the rest waits.
const MAX_READ_BYTES: u64 = 8 << 20;
/// Check a known peer's daemon this often; its socket file is checked every round.
const PROBE_EVERY_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The worktree's top directory, spelled as the agent's working directory is.
    pub top: String,
    /// The repo's common git dir, canonical, so worktrees of one repo compare equal.
    pub common: String,
}

/// Where `cwd` works. Outside git (or without git), both are `cwd`.
pub fn workspace(cwd: &Path) -> Workspace {
    let plain = Workspace {
        top: cwd.display().to_string(),
        common: cwd.display().to_string(),
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-prefix", "--git-common-dir"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return plain;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(prefix), Some(common)) = (lines.next(), lines.next()) else {
        return plain;
    };
    // The top as the agent spells it: cwd without the part below the top. Tool paths
    // use the agent's spelling, which may differ from git's (a symlinked home).
    let depth = Path::new(prefix.trim_end_matches('/')).components().count();
    let mut top = cwd.to_path_buf();
    for _ in 0..depth {
        top.pop();
    }
    let common = cwd.join(common);
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    Workspace {
        top: top.display().to_string(),
        common: common.display().to_string(),
    }
}

#[derive(Debug, Default)]
struct Tail {
    offset: u64,
    agent: String,
    workspace: Option<Workspace>,
    live: bool,
    probed_ms: u64,
}

/// Follows the ledgers of other live sessions for one session.
pub struct Watcher {
    me: PathBuf,
    my_common: String,
    tails: BTreeMap<PathBuf, Tail>,
}

impl Watcher {
    pub fn new(me: PathBuf, my_common: String) -> Self {
        Self {
            me,
            my_common,
            tails: BTreeMap::new(),
        }
    }

    /// One round over `root` (the sessions dir): new peer events, in session order.
    pub fn poll(&mut self, root: &Path, now: u64) -> Vec<Event> {
        let mut out = Vec::new();
        for dir in session_dirs(root) {
            if dir == self.me {
                continue;
            }
            let sock = socket_path(&dir).exists();
            let tail = match self.tails.get_mut(&dir) {
                Some(t) => t,
                // Never seen live: wait for its socket.
                None if !sock => continue,
                None => self.tails.entry(dir.clone()).or_default(),
            };
            let was_live = tail.live;
            if !sock {
                tail.live = false;
            } else if !tail.live || now.saturating_sub(tail.probed_ms) >= PROBE_EVERY_MS {
                tail.live = is_live(&dir);
                tail.probed_ms = now;
            }
            let session = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if tail.live {
                read_new(&dir, &session, tail, &self.my_common, &mut out);
            } else if was_live && tail.workspace.is_some() {
                out.push(peer(&session, 0, now, &tail.agent, PeerActivity::Left));
            }
        }
        out
    }
}

fn session_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn peer(session: &str, seq: u64, at_ms: u64, agent: &str, activity: PeerActivity) -> Event {
    Event::Peer {
        session: session.to_string(),
        seq,
        at_ms,
        agent: agent.to_string(),
        activity,
    }
}

/// Complete new lines of a peer's ledger, mapped to peer events.
fn read_new(dir: &Path, session: &str, tail: &mut Tail, my_common: &str, out: &mut Vec<Event>) {
    let Ok(mut f) = File::open(dir.join("ledger.jsonl")) else {
        return;
    };
    // A ledger shorter than what was read was rewritten: read it again.
    if f.metadata().is_ok_and(|m| m.len() < tail.offset) {
        tail.offset = 0;
    }
    if f.seek(SeekFrom::Start(tail.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.take(MAX_READ_BYTES).read_to_end(&mut buf).is_err() {
        return;
    }
    // Only whole lines: the daemon may be writing the last one.
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return;
    };
    tail.offset += end as u64 + 1;
    for line in buf[..end].split(|&b| b == b'\n') {
        // A line this build cannot read (another version) is skipped.
        let Ok(e) = serde_json::from_slice::<Entry>(line) else {
            continue;
        };
        if let Some(activity) = activity_of(&e, tail, my_common) {
            out.push(peer(session, e.seq, e.ts_ms, &tail.agent, activity));
        }
    }
}

/// What a peer's ledger entry means for others. Edits and git commands count only in
/// the same repo; the start counts everywhere (for the fleet view).
fn activity_of(e: &Entry, tail: &mut Tail, my_common: &str) -> Option<PeerActivity> {
    let Body::Event { event } = &e.body else {
        return None;
    };
    if let Event::SessionStarted { agent, cwd, .. } = event {
        let ws = workspace(Path::new(cwd));
        tail.agent = agent.clone();
        tail.workspace = Some(ws.clone());
        return Some(PeerActivity::Joined {
            cwd: cwd.clone(),
            top: ws.top,
            common: ws.common,
        });
    }
    if tail.workspace.as_ref()?.common != my_common {
        return None;
    }
    match event {
        Event::FileEdited { path, .. } | Event::FileChanged { path, .. } => {
            Some(PeerActivity::Edited { path: path.clone() })
        }
        Event::ToolCompleted {
            tool,
            input,
            failed: false,
            ..
        } if tool == "Bash" => {
            let cmd = input.get("command").and_then(|c| c.as_str())?;
            tend_core::fleet::git_effect(cmd).map(|command| PeerActivity::Git { command })
        }
        _ => None,
    }
}

/// What one session's ledger says, for `tend fleet`.
struct Row {
    id: String,
    agent: String,
    mode: String,
    workspace: Workspace,
    tools: u64,
    last_ms: u64,
    loops: u64,
    blocks: u64,
    notices: u64,
}

fn row(dir: &Path) -> Option<Row> {
    let entries = crate::store::read_entries(dir).ok()?;
    let mut r = Row {
        id: dir.file_name()?.to_string_lossy().to_string(),
        agent: "?".into(),
        mode: "?".into(),
        workspace: Workspace {
            top: String::new(),
            common: String::new(),
        },
        tools: 0,
        last_ms: 0,
        loops: 0,
        blocks: 0,
        notices: 0,
    };
    let mut cwd = None;
    for e in &entries {
        match &e.body {
            Body::Event { event } => match event {
                Event::Peer { .. } => continue,
                Event::SessionStarted {
                    agent,
                    cwd: c,
                    mode,
                    ..
                } => {
                    r.agent = agent.clone();
                    cwd = Some(c.clone());
                    if let Some(m) = mode {
                        r.mode = format!("{m:?}").to_lowercase();
                    }
                }
                Event::Workspace { top, common } => {
                    r.workspace = Workspace {
                        top: top.clone(),
                        common: common.clone(),
                    }
                }
                Event::ToolCompleted { .. } => r.tools += 1,
                _ => {}
            },
            Body::ModeChanged { to, .. } => r.mode = format!("{to:?}").to_lowercase(),
            Body::Action {
                action, applied, ..
            } => match action {
                ActionKind::Steer { .. } => r.loops += 1,
                ActionKind::Guard { decision, .. }
                    if *applied && *decision != tend_core::guard::GuardDecision::Warn =>
                {
                    r.blocks += 1
                }
                ActionKind::Fleet { .. } => r.notices += 1,
                _ => {}
            },
            _ => {}
        }
        r.last_ms = r.last_ms.max(e.ts_ms);
    }
    // A session with `fleet = off` records no workspace: find it now.
    if r.workspace.common.is_empty() {
        r.workspace = workspace(Path::new(cwd.as_deref()?));
    }
    Some(r)
}

/// `tend fleet`: the live tend sessions on this machine, grouped by repo.
pub fn view(root: &Path, now: u64) -> String {
    let mut rows: Vec<Row> = session_dirs(root)
        .iter()
        .filter(|d| socket_path(d).exists() && is_live(d))
        .filter_map(|d| row(d))
        .collect();
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("No live tend sessions.\n");
        return out;
    }
    rows.sort_by(|a, b| (&a.workspace.common, &a.id).cmp(&(&b.workspace.common, &b.id)));
    let n = rows.len();
    let _ = writeln!(
        out,
        "{n} live tend session{}",
        if n == 1 { "" } else { "s" }
    );
    let mut group = None;
    for r in &rows {
        if group != Some(&r.workspace.common) {
            group = Some(&r.workspace.common);
            let repo = r.workspace.common.trim_end_matches("/.git");
            let _ = writeln!(out, "\n{repo}");
        }
        let mut facts = vec![
            format!("{} tool calls", r.tools),
            format!("active {}", ago(now.saturating_sub(r.last_ms))),
        ];
        for (count, one, many) in [
            (r.loops, "loop", "loops"),
            (r.blocks, "blocked edit", "blocked edits"),
            (r.notices, "fleet notice", "fleet notices"),
        ] {
            if count > 0 {
                facts.push(format!("{count} {}", if count == 1 { one } else { many }));
            }
        }
        let _ = writeln!(
            out,
            "  {}  {}  {}  {}",
            r.id, r.agent, r.mode, r.workspace.top
        );
        let _ = writeln!(out, "      {}", facts.join(" · "));
    }
    out
}

fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

/// `tend fleet` with the default sessions dir.
pub fn view_default() -> String {
    view(&paths::sessions_dir(), paths::now_ms())
}

/// The entry `seq` of session `id`'s ledger, for `tend why` across sessions.
pub fn peer_entry(id: &str, seq: u64) -> Option<Entry> {
    let dir = paths::sessions_dir().join(id);
    crate::store::read_entries(&dir)
        .ok()?
        .into_iter()
        .find(|e| e.seq == seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tend-fleet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn workspaces_of_a_repo_its_worktree_and_a_plain_dir() {
        let d = tmp("ws");
        let repo = d.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
        );
        git(&repo, &["worktree", "add", "-q", "../wt"]);
        let main = workspace(&repo.join("src"));
        assert_eq!(main.top, repo.display().to_string());
        let wt = workspace(&d.join("wt"));
        assert_eq!(wt.top, d.join("wt").display().to_string());
        assert_eq!(wt.common, main.common, "worktrees share the common dir");
        let plain = tmp("ws-plain");
        assert_eq!(workspace(&plain).top, plain.display().to_string());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// A peer's ledger from the fleet e2e run, sanitized, plus two shell git calls.
    const PEER_LEDGER: &str = include_str!("../../../fixtures/fleet/peer_ledger.jsonl");

    #[test]
    fn peer_ledger_lines_become_peer_events() {
        let d = tmp("tail");
        let dir = d.join("peer");
        std::fs::create_dir_all(&dir).unwrap();
        // A torn last line waits for the next round.
        let cut = PEER_LEDGER.find("{\"seq\":10,").unwrap() + 20;
        std::fs::write(dir.join("ledger.jsonl"), &PEER_LEDGER[..cut]).unwrap();
        // /home/dev/shop does not exist here: its workspace is the plain directory.
        let common = "/home/dev/shop";
        let mut tail = Tail::default();
        let mut out = Vec::new();
        read_new(&dir, "peer", &mut tail, common, &mut out);
        let what: Vec<&PeerActivity> = out
            .iter()
            .filter_map(|e| match e {
                Event::Peer { activity, .. } => Some(activity),
                _ => None,
            })
            .collect();
        // The start and the edit; the peer's own peer events are never passed on.
        assert!(
            matches!(&what[..], [PeerActivity::Joined { top, .. }, PeerActivity::Edited { path }]
                if top == "/home/dev/shop" && path == "/home/dev/shop/src/api.rs"),
            "{what:?}"
        );
        assert!(matches!(&out[1], Event::Peer { seq: 4, agent, .. } if agent == "claude-code"));
        // The rest of the file arrives: `git stash` counts, `git stash list` does not.
        std::fs::write(dir.join("ledger.jsonl"), PEER_LEDGER).unwrap();
        out.clear();
        read_new(&dir, "peer", &mut tail, common, &mut out);
        assert!(
            matches!(&out[..], [Event::Peer { seq: 10, activity: PeerActivity::Git { command }, .. }] if command == "git stash"),
            "{out:?}"
        );
        // Another repo: the start counts, edits and git do not.
        let mut tail = Tail::default();
        out.clear();
        read_new(&dir, "peer", &mut tail, "/elsewhere/.git", &mut out);
        assert_eq!(out.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rewritten_peer_ledger_is_read_again_from_the_start() {
        let d = tmp("trunc");
        std::fs::write(d.join("ledger.jsonl"), "x\n".repeat(50)).unwrap();
        let mut tail = Tail::default();
        let mut out = Vec::new();
        read_new(&d, "s", &mut tail, "", &mut out);
        assert_eq!(tail.offset, 100);
        std::fs::write(d.join("ledger.jsonl"), "y\n").unwrap();
        read_new(&d, "s", &mut tail, "", &mut out);
        assert_eq!(tail.offset, 2, "the shorter file was read from 0");
    }
}
