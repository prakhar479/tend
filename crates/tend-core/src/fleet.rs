//! Fleet awareness (F1): what other tend sessions on this machine did that matters to
//! this one. Peers' activity comes in as `Event::Peer`; notices go out as data, and the
//! controller records and delivers them at this session's next hook.
//!
//! Rules (design: `docs/fleet-2026-10-21.md`):
//! - Peer edit: in this checkout, a peer edited a file after this session did.
//! - Peer git: in this checkout, a peer ran a git command that changes files or HEAD.
//! - Peer failure: a failed run here names a file a peer edited in this checkout recently.
//! - Worktree overlap: in another worktree of this repo, a peer edited a file this
//!   session edited too, so merging both branches will conflict there.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::event::PeerActivity;
use crate::perception::shell;

/// Notices delivered with one hook reply, at most.
const MAX_PER_HOOK: usize = 3;
/// Notices in one session, at most. Past this, fleet notices stop: more would be noise.
const MAX_NOTICES: usize = 40;
/// A peer's edit can explain a failure here for this long.
const FAILURE_WINDOW_MS: u64 = 30 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    PeerEdit,
    PeerGit,
    PeerFailure,
    WorktreeOverlap,
}

impl NoticeKind {
    pub fn label(self) -> &'static str {
        match self {
            NoticeKind::PeerEdit => "peer edit",
            NoticeKind::PeerGit => "peer git",
            NoticeKind::PeerFailure => "peer failure",
            NoticeKind::WorktreeOverlap => "worktree overlap",
        }
    }
}

/// How a peer's workspace relates to this session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    SameCheckout,
    Worktree,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub peer: String,
    pub path: Option<String>,
    /// What the agent reads.
    pub text: String,
    /// One sentence for the ledger.
    pub reason: String,
    /// The `Event::Peer` entry (this ledger's seq) the notice comes from.
    pub cause: u64,
    /// When the peer did it (the peer's clock).
    pub at_ms: u64,
}

#[derive(Debug, Clone)]
struct Peer {
    agent: String,
    top: String,
    common: String,
}

/// A peer's latest edit of one path.
#[derive(Debug, Clone, Copy)]
struct PeerEdit {
    at_ms: u64,
    cause: u64,
}

#[derive(Debug, Default)]
pub struct Fleet {
    /// This session's worktree top and common git dir.
    me: Option<(String, String)>,
    peers: BTreeMap<String, Peer>,
    /// Repo-relative paths this conversation edited, with the time of the last edit.
    my_edits: BTreeMap<String, u64>,
    /// (path, peer) → the peer's latest edit of it.
    peer_edits: BTreeMap<(String, String), PeerEdit>,
    pending: Vec<Notice>,
    /// (kind, peer, key) already noticed. The key is a path, or the entry for git.
    told: BTreeSet<(NoticeKind, String, String)>,
    sent: usize,
}

impl Fleet {
    pub fn set_workspace(&mut self, top: &str, common: &str) {
        self.me = Some((top.to_string(), common.to_string()));
    }

    pub fn relation(&self, peer: &str) -> Relation {
        match (&self.me, self.peers.get(peer)) {
            (Some((top, common)), Some(p)) => relation(top, common, &p.top, &p.common),
            _ => Relation::Other,
        }
    }

    /// A peer event, recorded in this ledger as entry `cause`.
    pub fn on_peer(
        &mut self,
        session: &str,
        agent: &str,
        at_ms: u64,
        activity: &PeerActivity,
        cause: u64,
    ) {
        match activity {
            PeerActivity::Joined { top, common, .. } => {
                self.peers.insert(
                    session.to_string(),
                    Peer {
                        agent: agent.to_string(),
                        top: top.clone(),
                        common: common.clone(),
                    },
                );
            }
            // A peer that left stays known: its edits still conflict with ours.
            PeerActivity::Left => {}
            PeerActivity::Edited { path } => self.on_peer_edit(session, path, at_ms, cause),
            PeerActivity::Git { command } => {
                if self.relation(session) != Relation::SameCheckout {
                    return;
                }
                let key = (NoticeKind::PeerGit, session.to_string(), cause.to_string());
                if self.told.insert(key) {
                    let who = self.who(session);
                    self.pending.push(Notice {
                        kind: NoticeKind::PeerGit,
                        peer: session.to_string(),
                        path: None,
                        text: format!(
                            "tend: {who} ran `{command}` in this checkout. Files or HEAD may have changed under you. Check `git status` before you go on."
                        ),
                        reason: format!(
                            "Session {session} ran a git command that changes this checkout."
                        ),
                        cause,
                        at_ms,
                    });
                }
            }
        }
    }

    fn on_peer_edit(&mut self, session: &str, path: &str, at_ms: u64, cause: u64) {
        let Some(peer) = self.peers.get(session) else {
            return;
        };
        let Some(rel) = relative(path, &peer.top) else {
            return;
        };
        let rel = rel.to_string();
        let rel_kind = self.relation(session);
        if rel_kind == Relation::Other {
            return;
        }
        self.peer_edits.insert(
            (rel.clone(), session.to_string()),
            PeerEdit { at_ms, cause },
        );
        let Some(&mine) = self.my_edits.get(&rel) else {
            return;
        };
        match rel_kind {
            Relation::SameCheckout if mine <= at_ms => {
                self.notice_peer_edit(session, &rel, at_ms, cause)
            }
            Relation::Worktree => self.notice_overlap(session, &rel, at_ms, cause),
            _ => {}
        }
    }

    fn notice_peer_edit(&mut self, session: &str, rel: &str, at_ms: u64, cause: u64) {
        let key = (NoticeKind::PeerEdit, session.to_string(), rel.to_string());
        if !self.told.insert(key) {
            return;
        }
        let who = self.who(session);
        self.pending.push(Notice {
            kind: NoticeKind::PeerEdit,
            peer: session.to_string(),
            path: Some(rel.to_string()),
            text: format!(
                "tend: {who} edited {rel} after you did. Read it again before you change it."
            ),
            reason: format!(
                "Session {session} edited {rel} in this checkout after this session edited it."
            ),
            cause,
            at_ms,
        });
    }

    fn notice_overlap(&mut self, session: &str, rel: &str, at_ms: u64, cause: u64) {
        let key = (
            NoticeKind::WorktreeOverlap,
            session.to_string(),
            rel.to_string(),
        );
        if !self.told.insert(key) {
            return;
        }
        let who = self.who(session);
        let top = self.peers.get(session).map_or("", |p| p.top.as_str());
        self.pending.push(Notice {
            kind: NoticeKind::WorktreeOverlap,
            peer: session.to_string(),
            path: Some(rel.to_string()),
            text: format!(
                "tend: {who} also changed {rel}, in its worktree {top}. Merging both branches will conflict in this file."
            ),
            reason: format!("Session {session} edited {rel} in another worktree of this repo."),
            cause,
            at_ms,
        });
    }

    /// This session edited `path` (absolute) at `now`.
    pub fn on_my_edit(&mut self, path: &str, now: u64) {
        let Some((top, _)) = &self.me else {
            return;
        };
        let Some(rel) = relative(path, top) else {
            return;
        };
        let rel = rel.to_string();
        self.my_edits.insert(rel.clone(), now);
        // An edit reads the file first (Claude Code checks it), so earlier peer-edit
        // notices for it are moot, and a later peer edit is news again.
        self.pending
            .retain(|n| !(n.kind == NoticeKind::PeerEdit && n.path.as_deref() == Some(&rel)));
        self.told
            .retain(|(k, _, p)| !(*k == NoticeKind::PeerEdit && *p == rel));
        let overlaps: Vec<(String, PeerEdit)> = self
            .peer_edits
            .iter()
            .filter(|((p, s), _)| *p == rel && self.relation(s) == Relation::Worktree)
            .map(|((_, s), e)| (s.clone(), *e))
            .collect();
        for (session, e) in overlaps {
            self.notice_overlap(&session, &rel, e.at_ms, e.cause);
        }
    }

    /// A failed run here. Queue a notice for each file the output names that a peer in
    /// this checkout edited recently, after this session's own last edit of it.
    pub fn on_my_failure(&mut self, output: &str, now: u64) {
        let found: Vec<(String, String, PeerEdit)> = self
            .peer_edits
            .iter()
            .filter(|((rel, session), e)| {
                self.relation(session) == Relation::SameCheckout
                    && now.saturating_sub(e.at_ms) <= FAILURE_WINDOW_MS
                    && self.my_edits.get(rel).is_none_or(|&mine| mine < e.at_ms)
                    && mentions(output, rel)
            })
            .map(|((rel, session), e)| (rel.clone(), session.clone(), *e))
            .collect();
        for (rel, session, e) in found {
            let key = (
                NoticeKind::PeerFailure,
                session.clone(),
                format!("{rel}@{}", e.cause),
            );
            if !self.told.insert(key) {
                continue;
            }
            let who = self.who(&session);
            let ago = now.saturating_sub(e.at_ms) / 60_000;
            let when = if ago == 0 {
                "under a minute ago".to_string()
            } else {
                format!("{ago} min ago")
            };
            self.pending.push(Notice {
                kind: NoticeKind::PeerFailure,
                peer: session.clone(),
                path: Some(rel.clone()),
                text: format!(
                    "tend: this failure names {rel}, which {who} edited {when}. It may come from that session's work, not yours."
                ),
                reason: format!(
                    "The failed run names {rel}, which session {session} edited recently in this checkout."
                ),
                cause: e.cause,
                at_ms: e.at_ms,
            });
        }
    }

    /// A new conversation in this session: its edits are not this one's.
    pub fn reset_conversation(&mut self) {
        self.my_edits.clear();
        self.pending.retain(|n| n.kind == NoticeKind::PeerGit);
        self.told.retain(|(k, _, _)| *k != NoticeKind::PeerEdit);
    }

    /// Notices to deliver now, oldest first, within the limits.
    pub fn take(&mut self) -> Vec<Notice> {
        let room = MAX_PER_HOOK.min(MAX_NOTICES.saturating_sub(self.sent));
        let n = room.min(self.pending.len());
        let out: Vec<Notice> = self.pending.drain(..n).collect();
        self.sent += out.len();
        if self.sent >= MAX_NOTICES {
            self.pending.clear();
        }
        out
    }

    fn who(&self, session: &str) -> String {
        match self.peers.get(session) {
            Some(p) => format!("session {session} ({})", p.agent),
            None => format!("session {session}"),
        }
    }
}

/// How workspace (top, common) of a peer relates to this one.
pub fn relation(top: &str, common: &str, peer_top: &str, peer_common: &str) -> Relation {
    if top == peer_top {
        Relation::SameCheckout
    } else if common == peer_common {
        Relation::Worktree
    } else {
        Relation::Other
    }
}

/// `path` relative to directory `top`, if it is inside it.
fn relative<'a>(path: &'a str, top: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(top)?.strip_prefix('/')?;
    (!rest.is_empty()).then_some(rest)
}

/// `output` names `rel` as a path: at the start, or after a separator, so `a.py` does
/// not match `data.py`.
fn mentions(output: &str, rel: &str) -> bool {
    output.match_indices(rel).any(|(i, _)| {
        output[..i].chars().next_back().is_none_or(|c| {
            matches!(c, '/' | '"' | '\'' | '(' | '[' | ':' | '`') || c.is_whitespace()
        })
    })
}

/// Git subcommands that change the working tree, the index, or HEAD.
const GIT_CHANGES: [&str; 14] = [
    "am",
    "apply",
    "checkout",
    "cherry-pick",
    "clean",
    "commit",
    "merge",
    "pull",
    "rebase",
    "reset",
    "restore",
    "revert",
    "stash",
    "switch",
];

/// The git command in `cmd` that changes the checkout, if any: `git stash`, `git
/// checkout main`, `git commit -am …`. Read-only uses (`git stash list`, `--help`) and
/// git words inside quotes or here-docs are not.
pub fn git_effect(cmd: &str) -> Option<String> {
    for seg in shell::parse(cmd).segments {
        let w = shell::program_words(&seg);
        if w.first().map(|f| shell::program_name(f)) != Some("git") {
            continue;
        }
        // Skip global options: `-C dir`, `-c k=v`, `--no-pager`, `--git-dir=…`.
        let mut i = 1;
        while let Some(a) = w.get(i) {
            match a.as_str() {
                "-C" | "-c" => i += 2,
                a if a.starts_with('-') => i += 1,
                _ => break,
            }
        }
        let Some(sub) = w.get(i).map(String::as_str) else {
            continue;
        };
        if !GIT_CHANGES.contains(&sub) || w.iter().any(|a| a == "--help" || a == "-h") {
            continue;
        }
        if sub == "stash" && matches!(w.get(i + 1).map(String::as_str), Some("list" | "show")) {
            continue;
        }
        if sub == "apply" && w.iter().any(|a| a == "--check" || a == "--stat") {
            continue;
        }
        return Some(crate::text::truncate_chars(&w.join(" "), 80));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOP: &str = "/repo";
    const COMMON: &str = "/repo/.git";

    fn fleet_with(peer_top: &str) -> Fleet {
        let mut f = Fleet::default();
        f.set_workspace(TOP, COMMON);
        f.on_peer(
            "p1",
            "claude",
            0,
            &PeerActivity::Joined {
                cwd: peer_top.into(),
                top: peer_top.into(),
                common: COMMON.into(),
            },
            1,
        );
        f
    }

    fn edited(f: &mut Fleet, path: &str, at: u64, cause: u64) {
        f.on_peer(
            "p1",
            "claude",
            at,
            &PeerActivity::Edited { path: path.into() },
            cause,
        );
    }

    fn kinds(f: &mut Fleet) -> Vec<NoticeKind> {
        f.take().into_iter().map(|n| n.kind).collect()
    }

    #[test]
    fn peer_edit_after_mine_is_noticed_once() {
        let mut f = fleet_with(TOP);
        f.on_my_edit("/repo/src/api.rs", 10);
        edited(&mut f, "/repo/src/api.rs", 20, 5);
        edited(&mut f, "/repo/src/api.rs", 30, 6);
        let n = f.take();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NoticeKind::PeerEdit);
        assert_eq!(n[0].cause, 5);
        assert!(n[0].text.contains("src/api.rs"), "{}", n[0].text);
        // My next edit re-reads the file; a later peer edit is news again.
        f.on_my_edit("/repo/src/api.rs", 40);
        edited(&mut f, "/repo/src/api.rs", 50, 7);
        assert_eq!(kinds(&mut f), [NoticeKind::PeerEdit]);
    }

    #[test]
    fn peer_edit_of_a_file_i_never_touched_says_nothing() {
        let mut f = fleet_with(TOP);
        f.on_my_edit("/repo/src/a.rs", 10);
        edited(&mut f, "/repo/src/b.rs", 20, 5);
        assert!(f.take().is_empty());
    }

    #[test]
    fn my_edit_after_the_peer_drops_the_pending_notice() {
        let mut f = fleet_with(TOP);
        f.on_my_edit("/repo/x.py", 10);
        edited(&mut f, "/repo/x.py", 20, 5);
        f.on_my_edit("/repo/x.py", 30);
        assert!(f.take().is_empty());
    }

    #[test]
    fn worktree_overlap_either_order() {
        // Peer first, then me.
        let mut f = fleet_with("/wt/b");
        edited(&mut f, "/wt/b/src/api.rs", 10, 5);
        assert!(f.take().is_empty());
        f.on_my_edit("/repo/src/api.rs", 20);
        let n = f.take();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NoticeKind::WorktreeOverlap);
        assert_eq!(n[0].path.as_deref(), Some("src/api.rs"));
        assert!(n[0].text.contains("/wt/b"), "{}", n[0].text);
        // Once per path and peer.
        edited(&mut f, "/wt/b/src/api.rs", 30, 6);
        f.on_my_edit("/repo/src/api.rs", 40);
        assert!(f.take().is_empty());
        // Me first, then the peer.
        let mut f = fleet_with("/wt/b");
        f.on_my_edit("/repo/src/api.rs", 10);
        edited(&mut f, "/wt/b/src/api.rs", 20, 5);
        assert_eq!(kinds(&mut f), [NoticeKind::WorktreeOverlap]);
    }

    #[test]
    fn other_repos_and_paths_outside_the_checkout_are_ignored() {
        let mut f = Fleet::default();
        f.set_workspace(TOP, COMMON);
        f.on_peer(
            "p1",
            "claude",
            0,
            &PeerActivity::Joined {
                cwd: "/other".into(),
                top: "/other".into(),
                common: "/other/.git".into(),
            },
            1,
        );
        f.on_my_edit("/repo/a.rs", 1);
        edited(&mut f, "/other/a.rs", 2, 5);
        f.on_peer(
            "p1",
            "claude",
            3,
            &PeerActivity::Git {
                command: "git stash".into(),
            },
            6,
        );
        let mut g = fleet_with(TOP);
        g.on_my_edit("/repo/a.rs", 1);
        edited(&mut g, "/elsewhere/a.rs", 2, 5);
        assert!(f.take().is_empty());
        assert!(g.take().is_empty());
    }

    #[test]
    fn peer_git_in_this_checkout_is_noticed_per_command() {
        let mut f = fleet_with(TOP);
        let git = |f: &mut Fleet, cause| {
            f.on_peer(
                "p1",
                "claude",
                5,
                &PeerActivity::Git {
                    command: "git stash".into(),
                },
                cause,
            )
        };
        git(&mut f, 5);
        git(&mut f, 5);
        git(&mut f, 6);
        let n = f.take();
        assert_eq!(n.len(), 2);
        assert!(n[0].text.contains("`git stash`"), "{}", n[0].text);
        // In another worktree, git there does not touch this checkout.
        let mut f = fleet_with("/wt/b");
        git(&mut f, 5);
        assert!(f.take().is_empty());
    }

    #[test]
    fn failure_naming_a_recent_peer_edit_is_noticed() {
        let mut f = fleet_with(TOP);
        edited(&mut f, "/repo/src/api.rs", 60_000, 5);
        f.on_my_failure(
            "error[E0308]: mismatched types\n --> src/api.rs:12:5",
            120_000,
        );
        let n = f.take();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NoticeKind::PeerFailure);
        assert!(n[0].text.contains("1 min ago"), "{}", n[0].text);
        // The same failure again says nothing new.
        f.on_my_failure("src/api.rs:12:5", 130_000);
        assert!(f.take().is_empty());
    }

    #[test]
    fn failure_rule_needs_a_recent_edit_a_path_match_and_not_my_own_edit() {
        // Too old.
        let mut f = fleet_with(TOP);
        edited(&mut f, "/repo/src/api.rs", 0, 5);
        f.on_my_failure("src/api.rs:1", FAILURE_WINDOW_MS + 1);
        assert!(f.take().is_empty());
        // A different file whose name ends the same.
        let mut f = fleet_with(TOP);
        edited(&mut f, "/repo/a.py", 0, 5);
        f.on_my_failure("File \"/repo/data.py\", line 3", 1);
        assert!(f.take().is_empty());
        // I edited it after the peer: the failure is more likely mine.
        let mut f = fleet_with(TOP);
        edited(&mut f, "/repo/a.py", 0, 5);
        f.on_my_edit("/repo/a.py", 10);
        f.on_my_failure("File \"/repo/a.py\", line 3", 20);
        assert!(f.take().is_empty());
    }

    #[test]
    fn delivery_is_limited_per_hook_and_per_session() {
        let mut f = fleet_with(TOP);
        for i in 0..50u64 {
            f.on_peer(
                "p1",
                "claude",
                i,
                &PeerActivity::Git {
                    command: "git commit".into(),
                },
                i,
            );
        }
        let mut total = 0;
        loop {
            let n = f.take();
            if n.is_empty() {
                break;
            }
            assert!(n.len() <= MAX_PER_HOOK);
            total += n.len();
        }
        assert_eq!(total, MAX_NOTICES);
    }

    #[test]
    fn new_conversation_forgets_my_edits() {
        let mut f = fleet_with(TOP);
        f.on_my_edit("/repo/a.rs", 1);
        f.reset_conversation();
        edited(&mut f, "/repo/a.rs", 2, 5);
        assert!(f.take().is_empty());
    }

    #[test]
    fn git_effects() {
        for (cmd, want) in [
            ("git stash", Some("git stash")),
            ("git stash pop", Some("git stash pop")),
            (
                "git -C ../x checkout main",
                Some("git -C ../x checkout main"),
            ),
            ("cd a && git commit -am 'wip'", Some("git commit -am wip")),
            (
                "git --no-pager reset --hard HEAD~1",
                Some("git --no-pager reset --hard HEAD~1"),
            ),
            ("git stash list", None),
            ("git status && git diff", None),
            ("git log --oneline", None),
            ("git checkout --help", None),
            ("git apply --check p.diff", None),
            ("echo 'git stash'", None),
            ("grep -r 'git reset' .", None),
        ] {
            assert_eq!(git_effect(cmd).as_deref(), want, "{cmd}");
        }
    }

    #[test]
    fn mentions_needs_a_path_boundary() {
        assert!(mentions("src/api.rs:3", "src/api.rs"));
        assert!(mentions("  --> src/api.rs:3", "src/api.rs"));
        assert!(mentions("File \"/repo/src/api.rs\"", "src/api.rs"));
        assert!(!mentions("xsrc/api.rs", "src/api.rs"));
        assert!(!mentions("data.py", "a.py"));
    }
}
