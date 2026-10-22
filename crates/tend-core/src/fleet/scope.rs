//! Fleet write scopes (F5c): what a role may edit, and the paths other sessions in the
//! same worktree claimed for their tasks. The session's daemon reads them (the hub
//! writes them); the controller denies an edit outside them in PreToolUse.

use serde::{Deserialize, Serialize};

use crate::policy::Glob;

/// A role's write rules, as the hub hands them to a session (`TEND_FLEET_RULES`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RulesFile {
    /// The session's worktree top: globs are relative to it.
    pub root: String,
    pub slot: String,
    #[serde(default)]
    pub may_write: Vec<String>,
    #[serde(default)]
    pub may_not_write: Vec<String>,
    /// Where the hub keeps the fleet's claims.
    #[serde(default)]
    pub claims: Option<String>,
    /// Where the hub puts messages for this session (F4).
    #[serde(default)]
    pub inbox: Option<String>,
}

/// One working task's scope, in one worktree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimFile {
    pub slot: String,
    pub task: String,
    pub worktree: String,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub root: String,
    pub slot: String,
    may_write: Vec<Glob>,
    may_not_write: Vec<Glob>,
    claims: Vec<(String, String, Vec<Glob>)>,
}

impl Rules {
    pub fn new(f: &RulesFile) -> Self {
        let globs = |v: &[String]| v.iter().filter_map(|g| Glob::new(g)).collect();
        Rules {
            root: lexical(&f.root).trim_end_matches('/').to_string(),
            slot: f.slot.clone(),
            may_write: globs(&f.may_write),
            may_not_write: globs(&f.may_not_write),
            claims: Vec::new(),
        }
    }

    /// The claims of other sessions working in this worktree.
    pub fn set_claims(&mut self, all: &[ClaimFile]) {
        self.claims = all
            .iter()
            .filter(|c| {
                c.slot != self.slot && lexical(&c.worktree).trim_end_matches('/') == self.root
            })
            .map(|c| {
                let globs = c.scope.iter().filter_map(|g| Glob::new(g)).collect();
                (c.slot.clone(), c.task.clone(), globs)
            })
            .collect();
    }

    /// Why a shell command may not write `path`, if it may not (G4). Only what is
    /// denied counts: a shell command's words are read loosely (`s/a/b/` of `sed -i`
    /// reads as a path), so an allow-list (`may_write`) would refuse words that are no
    /// files. Advisory, like the guard's Bash rule.
    pub fn refuse_shell_write(&self, path: &str) -> Option<String> {
        let mut deny_only = self.clone();
        deny_only.may_write.clear();
        deny_only.refuse(path)
    }

    /// Why this session may not edit `path`, if it may not.
    pub fn refuse(&self, path: &str) -> Option<String> {
        // `/wt/team-2/x` is not inside `/wt/team`.
        let path = &lexical(path);
        let rel = path
            .strip_prefix(&self.root)
            .filter(|r| r.is_empty() || r.starts_with('/') || self.root.ends_with('/'))
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(path);
        if let Some(g) = self.may_not_write.iter().find(|g| g.matches(rel)) {
            return Some(format!("its role may not write `{}`", g.pattern));
        }
        if !self.may_write.is_empty() && !self.may_write.iter().any(|g| g.matches(rel)) {
            let allowed: Vec<&str> = self.may_write.iter().map(|g| g.pattern.as_str()).collect();
            return Some(format!("its role may write only {}", allowed.join(", ")));
        }
        for (slot, task, globs) in &self.claims {
            if let Some(g) = globs.iter().find(|g| g.matches(rel)) {
                return Some(format!(
                    "{slot} claimed `{}` for {task} in this worktree",
                    g.pattern
                ));
            }
        }
        None
    }
}

/// `path` without `.`, empty, and `x/..` parts, so `src/../tests/a` is `tests/a`. Parts
/// of a relative path that climb out stay.
pub(super) fn lexical(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." if parts.last().is_some_and(|l| *l != "..") => {
                parts.pop();
            }
            ".." if path.starts_with('/') => {}
            p => parts.push(p),
        }
    }
    let body = parts.join("/");
    if path.starts_with('/') {
        format!("/{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(may: &[&str], may_not: &[&str]) -> Rules {
        Rules::new(&RulesFile {
            root: "/wt/team".into(),
            slot: "worker-1".into(),
            may_write: may.iter().map(|s| s.to_string()).collect(),
            may_not_write: may_not.iter().map(|s| s.to_string()).collect(),
            claims: None,
            inbox: None,
        })
    }

    #[test]
    fn role_scopes() {
        let r = rules(&[], &["tests/**"]);
        assert!(r
            .refuse("/wt/team/tests/test_api.py")
            .is_some_and(|w| w.contains("may not write `tests/**`")));
        assert!(r.refuse("/wt/team/src/api.py").is_none());
        let r = rules(&["tests/**"], &[]);
        assert!(r
            .refuse("/wt/team/src/api.py")
            .is_some_and(|w| w.contains("may write only tests/**")));
        assert!(r.refuse("/wt/team/tests/a/b.py").is_none());
    }

    #[test]
    fn a_sibling_dir_is_not_inside_the_root() {
        // `/wt/team-2` shares a prefix with `/wt/team`: its files are not `-2/…`.
        let r = rules(&[], &["-2/**"]);
        assert!(r.refuse("/wt/team-2/src/a.py").is_none());
        assert!(r.refuse("/wt/team/-2/a.py").is_some());
    }

    #[test]
    fn shell_writes_see_only_what_is_denied() {
        let r = rules(&["tests/**"], &["migrations/**"]);
        assert!(r.refuse_shell_write("migrations/0001.sql").is_some());
        assert!(
            r.refuse_shell_write("s/a/b/").is_none(),
            "a sed script is no file"
        );
        assert!(
            r.refuse("/wt/team/src/x.py").is_some(),
            "the edit tools still use the allow-list"
        );
    }

    #[test]
    fn claims_of_others_in_this_worktree() {
        let mut r = rules(&[], &[]);
        let claim = |slot: &str, wt: &str| ClaimFile {
            slot: slot.into(),
            task: "t-2".into(),
            worktree: wt.into(),
            scope: vec!["src/api/**".into()],
        };
        r.set_claims(&[
            claim("worker-2", "/wt/team"),
            claim("worker-3", "/wt/other"),
            claim("worker-1", "/wt/team"),
        ]);
        assert!(r
            .refuse("/wt/team/src/api/mod.rs")
            .is_some_and(|w| w.contains("worker-2 claimed `src/api/**` for t-2")));
        assert!(r.refuse("/wt/team/src/db.rs").is_none());
        assert_eq!(r.claims.len(), 1, "not its own, not another worktree's");
    }

    #[test]
    fn a_claims_worktree_is_compared_lexically() {
        let mut r = rules(&[], &[]);
        for wt in ["/wt//team", "/wt/team/.", "/wt/x/../team/"] {
            r.set_claims(&[ClaimFile {
                slot: "worker-2".into(),
                task: "t-2".into(),
                worktree: wt.into(),
                scope: vec!["src/**".into()],
            }]);
            assert!(r.refuse("/wt/team/src/a.rs").is_some(), "{wt}");
        }
    }

    #[test]
    fn dots_and_doubled_slashes_do_not_hide_a_path() {
        let r = rules(&[], &["tests/**"]);
        for p in [
            "/wt/team/src/../tests/a.py",
            "/wt/team/./tests/a.py",
            "/wt/team//tests/a.py",
            "./tests/a.py",
            "src/../tests/a.py",
        ] {
            assert!(r.refuse(p).is_some(), "{p}");
        }
        assert!(r.refuse("/wt/team/tests/../src/a.py").is_none());
        assert!(r.refuse("/wt/team/../team-2/tests/a.py").is_none());
    }
}
