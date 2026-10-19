//! End-to-end: launcher → daemon → shim → connector → controller → ledger,
//! with a fake agent script in place of Claude Code.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rein")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude_code")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rein-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A fake agent: records its arguments, sends two hook calls through `rein hook`, exits.
/// `rein` must be on PATH: the launcher puts its own directory first.
fn fake_agent(dir: &Path) -> PathBuf {
    let script = dir.join("fake-claude.sh");
    let body = format!(
        "#!/bin/sh\n\
         echo \"$@\" > '{d}/args.txt'\n\
         rein hook PostToolUse < '{post}' > '{d}/post-out.json'\n\
         rein hook PreToolUse < '{pre}' > '{d}/pre-out.json'\n\
         rein hook PostToolUseFailure < '{fail}' > '{d}/fail-out.json'\n\
         rein allow > '{d}/allow-out.txt' 2>&1\n\
         rein hook PreToolUse < '{pre}' > '{d}/retry-out.json'\n",
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
        .args(["claude", "--rein-mode", mode, "--resume", "abc"])
        .env("REIN_HOME", &home)
        .env("REIN_CLAUDE_BIN", fake_agent(&dir))
        .env("REIN_HOOK_TIMEOUT_MS", "2000")
        .env("REIN_CONFIG", &config)
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

fn rein(args: &[&str], session: &Path) -> String {
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
        if !session.join("reind.sock").exists() {
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

    let report = rein(&["report"], &session);
    assert!(report.contains("mode: look"), "{report}");
    assert!(
        report.contains("Config: loop_threshold = 4 (default 3)"),
        "{report}"
    );
    assert!(report.contains("Shaping: 1 outputs"), "{report}");
    assert!(report.contains("[would apply]"), "{report}");
    // Look mode blocks nothing, so the retry is flagged again and there is nothing to allow.
    assert!(report.contains("Test guard: 2"), "{report}");
    assert!(report.contains("[would deny]"), "{report}");
    assert!(fs::read_to_string(dir.join("allow-out.txt"))
        .unwrap()
        .contains("No edit was blocked"));
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
    assert!(summary.contains("rein show o-1"));
    // S1: a failing run arrives as PostToolUseFailure, so the captured PostToolUse run passes.
    assert!(summary.contains("300 passed"), "{summary}");

    let pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("pre-out.json")).unwrap()).unwrap();
    // S6: Claude Code's ask prompt hides the reason, so the guard denies with it.
    assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(pre["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("rein allow"));
    let retry = fs::read_to_string(dir.join("retry-out.json")).unwrap();
    assert_eq!(retry, "", "allowed once by the user: no response on retry");
    assert!(fs::read_to_string(dir.join("allow-out.txt"))
        .unwrap()
        .contains("Allowed once"));

    let original: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    assert_eq!(
        rein(&["show", "o-1"], &session),
        original["tool_response"]["stdout"].as_str().unwrap()
    );

    // S2: PostToolUseFailure ignores updatedToolOutput, so a failed run is never replaced.
    assert_eq!(fs::read_to_string(dir.join("fail-out.json")).unwrap(), "");

    let why = rein(&["why", "a-1"], &session);
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
        .env("REIN_CAPTURE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = session.join("reind.sock");
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
                        .env("REIN_SOCKET", &sock)
                        .env("REIN_HOOK_TIMEOUT_MS", "5000")
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

/// A daemon that is killed when the test ends, even on a panic. Its output is detached:
/// a daemon left holding the test's stdout makes cargo wait forever.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a daemon for a session dir. Returns the guard and its socket.
fn start_daemon(session: &Path, mode: &str) -> (Daemon, PathBuf) {
    fs::create_dir_all(session).unwrap();
    let child = Command::new(bin())
        .args(["daemon", "--session-dir"])
        .arg(session)
        .args(["--mode", mode])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let sock = session.join("reind.sock");
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    (Daemon(child), sock)
}

fn hook(sock: &Path, event: &str, payload: &serde_json::Value) -> String {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(["hook", event])
        .env("REIN_SOCKET", sock)
        .env("REIN_HOOK_TIMEOUT_MS", "5000")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
}

#[test]
fn failing_test_run_is_summarized_through_rein_run() {
    let dir = tmp("wrap");
    let session = dir.join("session");
    let (daemon, sock) = start_daemon(&session, "optimize");

    let pytest_out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/outputs/pytest_fail.txt");
    let command = format!("cat '{}' && false # pytest -v", pytest_out.display());
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    pre["hook_event_name"] = "PreToolUse".into();
    pre["permission_mode"] = "auto".into();
    pre["tool_input"]["command"] = command.clone().into();
    pre.as_object_mut().unwrap().remove("tool_response");

    // 1. PreToolUse: rein rewrites the command.
    let out: serde_json::Value = serde_json::from_str(&hook(&sock, "PreToolUse", &pre)).unwrap();
    let wrapped = out["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .expect("updatedInput")
        .to_string();
    // R0.4: the absolute rein binary, not whatever `rein` the agent's PATH finds.
    let exe = fs::canonicalize(bin()).unwrap();
    assert!(
        wrapped.starts_with(&format!("'{}' run --id 'toolu_01' --sh ", exe.display())),
        "{wrapped}"
    );

    // 2. Claude Code runs the new command in its shell.
    let bin_dir = Path::new(bin()).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    let ran = Command::new("/bin/sh")
        .arg("-c")
        .arg(&wrapped)
        .env("PATH", path)
        .env("REIN_SOCKET", &sock)
        .output()
        .unwrap();
    assert_eq!(ran.status.code(), Some(1), "exit code kept");
    let shown = String::from_utf8(ran.stdout).unwrap();
    assert!(shown.contains("rein show o-1"), "{shown}");
    assert!(
        shown.contains("FAILED tests/test_api.py::test_get_order"),
        "{shown}"
    );
    assert!(shown.len() * 3 < fs::read_to_string(&pytest_out).unwrap().len());

    // 3. PostToolUseFailure carries what the agent saw.
    let mut post = pre.clone();
    post["hook_event_name"] = "PostToolUseFailure".into();
    post["tool_input"]["command"] = wrapped.into();
    post["error"] = format!("Exit code 1\n{shown}").into();
    assert_eq!(hook(&sock, "PostToolUseFailure", &post), "");
    drop(daemon);

    let report = rein(&["report"], &session);
    assert!(report.contains("Wrapped test runs: 1"), "{report}");
    assert!(report.contains("Shaping: 1 outputs"), "{report}");
    assert!(report.contains("[applied]"), "{report}");
    assert!(!report.contains("Not shapeable"), "{report}");
    assert_eq!(
        rein(&["show", "o-1"], &session),
        fs::read_to_string(&pytest_out).unwrap(),
        "rein show gives the full original"
    );
}

/// R0.1 (D2): a stop (Claude Code's Bash timeout, Esc) prints what the command produced
/// so far and leaves no process behind.
#[test]
fn stopped_rein_run_prints_partial_output_and_leaves_no_orphans() {
    let marker = format!("sleep 37.{}", std::process::id());
    let child = Command::new(bin())
        .args(["run", "--id", "t", "--sh"])
        .arg(format!("echo started; {marker}; echo never"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    // SAFETY: signals the child we spawned.
    unsafe { libc_kill(child.id() as i32, 15) };
    let out = child.wait_with_output().unwrap();
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.starts_with("started\n"), "{shown}");
    assert_eq!(out.status.code(), Some(128 + 15));
    thread::sleep(Duration::from_millis(200));
    let left = Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "orphan: {}",
        String::from_utf8_lossy(&left.stdout)
    );
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// R0.3 (D4): a large filter must not stall pings or hooks from parallel tool calls.
#[test]
fn large_filter_does_not_block_pings_or_hooks() {
    let dir = tmp("busy");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    pre["hook_event_name"] = "PreToolUse".into();
    pre["permission_mode"] = "auto".into();
    pre["tool_input"]["command"] = "pytest -v".into();
    pre.as_object_mut().unwrap().remove("tool_response");
    assert!(hook(&sock, "PreToolUse", &pre).contains("updatedInput"));

    // ~8 MiB of failing pytest output, reported by rein run.
    let big = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/outputs/pytest_fail.txt"),
    )
    .unwrap()
    .repeat(300);
    let filter = {
        let sock = sock.clone();
        thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
            let req =
                serde_json::json!({ "type": "filter", "tool_use_id": "toolu_01", "output": big });
            writeln!(s, "{req}").unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            line
        })
    };
    thread::sleep(Duration::from_millis(100));
    let mut slowest = Duration::ZERO;
    let mut ls = pre.clone();
    ls["tool_input"]["command"] = "ls".into();
    ls["tool_use_id"] = "toolu_02".into();
    while !filter.is_finished() {
        let t = std::time::Instant::now();
        hook(&sock, "PreToolUse", &ls);
        slowest = slowest.max(t.elapsed());
        thread::sleep(Duration::from_millis(20));
    }
    let reply = filter.join().unwrap();
    assert!(
        reply.contains("rein show o-1"),
        "{}",
        &reply[..reply.len().min(200)]
    );
    // A hook call spawns the shim process; allow for that, but not for the filter.
    assert!(
        slowest < Duration::from_millis(250),
        "hook waited {slowest:?}"
    );
}
