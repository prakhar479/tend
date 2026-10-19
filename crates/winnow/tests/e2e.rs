//! End-to-end: launcher → daemon → shim → connector → controller → ledger,
//! with a fake agent script in place of Claude Code.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_winnow")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude_code")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("winnow-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A fake agent: records its arguments, sends two hook calls through `winnow hook`, exits.
/// `winnow` must be on PATH: the launcher puts its own directory first.
fn fake_agent(dir: &Path) -> PathBuf {
    let script = dir.join("fake-claude.sh");
    let body = format!(
        "#!/bin/sh\n\
         echo \"$@\" > '{d}/args.txt'\n\
         winnow hook PostToolUse < '{post}' > '{d}/post-out.json'\n\
         winnow hook PreToolUse < '{pre}' > '{d}/pre-out.json'\n\
         winnow hook PostToolUseFailure < '{fail}' > '{d}/fail-out.json'\n",
        d = dir.display(),
        post = fixture("post_tool_use_bash.json").display(),
        pre = fixture("pre_tool_use_edit_test.json").display(),
        fail = fixture("post_tool_use_failure_bash.json").display(),
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn run_session(name: &str, mode: &str) -> (PathBuf, PathBuf) {
    let dir = tmp(name);
    let home = dir.join("home");
    let config = dir.join("config.toml");
    fs::write(&config, "loop_threshold = 4\n").unwrap();
    let status = Command::new(bin())
        .args(["claude", "--winnow-mode", mode, "--resume", "abc"])
        .env("WINNOW_HOME", &home)
        .env("WINNOW_CLAUDE_BIN", fake_agent(&dir))
        .env("WINNOW_HOOK_TIMEOUT_MS", "2000")
        .env("WINNOW_CONFIG", &config)
        .status()
        .unwrap();
    assert!(status.success());
    let session = fs::read_dir(home.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    (dir, session)
}

fn winnow(args: &[&str], session: &Path) -> String {
    let out = Command::new(bin())
        .args(args)
        .arg("--session")
        .arg(session)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn socket_gone(session: &Path) -> bool {
    for _ in 0..50 {
        if !session.join("winnowd.sock").exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn look_mode_records_and_changes_nothing() {
    let (dir, session) = run_session("look", "look");
    let args = fs::read_to_string(dir.join("args.txt")).unwrap();
    assert!(
        args.starts_with("--settings ") && args.trim_end().ends_with("--resume abc"),
        "{args}"
    );
    assert_eq!(fs::read_to_string(dir.join("post-out.json")).unwrap(), "");
    assert_eq!(fs::read_to_string(dir.join("pre-out.json")).unwrap(), "");

    let report = winnow(&["report"], &session);
    assert!(report.contains("mode: look"), "{report}");
    assert!(
        report.contains("Config: loop_threshold = 4 (default 3)"),
        "{report}"
    );
    assert!(report.contains("Shaping: 1 outputs"), "{report}");
    assert!(report.contains("[would apply]"), "{report}");
    assert!(report.contains("Test guard: 1"), "{report}");
    assert!(report.contains("Not shapeable: 1 outputs"), "{report}");
    assert!(socket_gone(&session), "daemon must stop with the session");
}

#[test]
fn optimize_mode_shapes_and_guards() {
    let (dir, session) = run_session("optimize", "optimize");
    let post: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("post-out.json")).unwrap()).unwrap();
    let summary = post["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
        .as_str()
        .unwrap();
    assert!(summary.contains("winnow show o-1"));
    // S1: a failing run arrives as PostToolUseFailure, so the captured PostToolUse run passes.
    assert!(summary.contains("300 passed"), "{summary}");

    let pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("pre-out.json")).unwrap()).unwrap();
    assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "ask");

    let original: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    assert_eq!(
        winnow(&["show", "o-1"], &session),
        original["tool_response"]["stdout"].as_str().unwrap()
    );

    // S2: PostToolUseFailure ignores updatedToolOutput, so a failed run is never replaced.
    assert_eq!(fs::read_to_string(dir.join("fail-out.json")).unwrap(), "");

    let why = winnow(&["why", "a-1"], &session);
    assert!(why.contains("action a-1 (shape, applied)"), "{why}");
    assert!(why.contains("caused by"), "{why}");
}

#[test]
fn concurrent_hooks_keep_the_ledger_ordered_and_whole() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tmp("concurrent");
    let session = dir.join("session");
    fs::create_dir_all(&session).unwrap();
    let mut daemon = Command::new(bin())
        .args(["daemon", "--session-dir"])
        .arg(&session)
        .args(["--mode", "optimize"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = session.join("winnowd.sock");
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let calls = [
        ("PostToolUse", "post_tool_use_bash.json"),
        ("PostToolUseFailure", "post_tool_use_failure_bash.json"),
        ("PreToolUse", "pre_tool_use_edit_test.json"),
        ("PostToolUse", "post_tool_use_edit.json"),
    ];
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let sock = sock.clone();
            thread::spawn(move || {
                for i in 0..5 {
                    let (event, name) = calls[(t + i) % calls.len()];
                    let mut child = Command::new(bin())
                        .args(["hook", event])
                        .env("WINNOW_SOCKET", &sock)
                        .env("WINNOW_HOOK_TIMEOUT_MS", "5000")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .spawn()
                        .unwrap();
                    let payload = fs::read(fixture(name)).unwrap();
                    child.stdin.take().unwrap().write_all(&payload).unwrap();
                    assert!(child.wait().unwrap().success(), "hook exits 0");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let _ = daemon.kill();
    let _ = daemon.wait();

    let ledger = fs::read_to_string(session.join("ledger.jsonl")).unwrap();
    let hooks = fs::read_to_string(session.join("hooks.jsonl")).unwrap();
    assert_eq!(hooks.lines().count(), 40, "every call was captured");
    let mut last: Option<u64> = None;
    for (n, line) in ledger.lines().enumerate() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("torn ledger line {}: {e}", n + 1));
        let seq = v["seq"].as_u64().unwrap();
        assert!(last.is_none_or(|l| seq > l), "seq {seq} after {last:?}");
        last = Some(seq);
    }
    assert_eq!(last, Some(ledger.lines().count() as u64 - 1), "no gaps");
}
