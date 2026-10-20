//! End-to-end: launcher → daemon → shim → connector → controller → ledger,
//! with a fake agent script in place of Claude Code.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tend")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude_code")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tend-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A fake agent: records its arguments, sends two hook calls through `tend hook`, exits.
/// `tend` must be on PATH: the launcher puts its own directory first.
fn fake_agent(dir: &Path) -> PathBuf {
    let script = dir.join("fake-claude.sh");
    let body = format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
         echo \"$@\" > '{d}/args.txt'\n\
         tend hook PostToolUse < '{post}' > '{d}/post-out.json'\n\
         tend hook PreToolUse < '{pre}' > '{d}/pre-out.json'\n\
         tend hook PostToolUseFailure < '{fail}' > '{d}/fail-out.json'\n\
         tend allow > '{d}/allow-out.txt' 2>&1\n\
         tend hook PreToolUse < '{pre}' > '{d}/retry-out.json'\n",
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
        .args(["claude", "--tend-mode", mode, "--resume", "abc"])
        .env("TEND_HOME", &home)
        .env("TEND_CLAUDE_BIN", fake_agent(&dir))
        .env("TEND_HOOK_TIMEOUT_MS", "2000")
        .env("TEND_CONFIG", &config)
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

fn tend(args: &[&str], session: &Path) -> String {
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

/// The session socket, by the daemon's rule (`paths::socket_path`): a long session path
/// falls back to `<tmp>/tend-<uid>/<hash>.sock`, and to `/tmp` when that is long too.
fn socket_of(session: &Path) -> PathBuf {
    let p = session.join("tendd.sock");
    if p.as_os_str().len() <= 100 {
        return p;
    }
    let hash = tend_core::text::sha256_hex(&session.to_string_lossy());
    let uid = unsafe { libc::getuid() };
    let name = format!("{}.sock", &hash[..16]);
    let in_temp = std::env::temp_dir().join(format!("tend-{uid}")).join(&name);
    if in_temp.as_os_str().len() <= 100 {
        return in_temp;
    }
    PathBuf::from("/tmp").join(format!("tend-{uid}")).join(name)
}

/// A socket path for a test listener that fits the OS limit whatever $TMPDIR is.
fn short_socket(name: &str) -> PathBuf {
    let p = PathBuf::from("/tmp").join(format!("tend-t-{}-{name}.sock", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn socket_gone(session: &Path) -> bool {
    for _ in 0..50 {
        if !socket_of(session).exists() {
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

    let report = tend(&["report"], &session);
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
    assert!(summary.contains("tend show o-1"));
    // S1: a failing run arrives as PostToolUseFailure, so the captured PostToolUse run passes.
    assert!(summary.contains("300 passed"), "{summary}");

    let pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("pre-out.json")).unwrap()).unwrap();
    // S6: Claude Code's ask prompt hides the reason, so the guard denies with it.
    assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(pre["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("tend allow"));
    let retry = fs::read_to_string(dir.join("retry-out.json")).unwrap();
    assert_eq!(retry, "", "allowed once by the user: no response on retry");
    assert!(fs::read_to_string(dir.join("allow-out.txt"))
        .unwrap()
        .contains("Allowed once"));
    // R2.3: the flag the user lifted counts against the guard's precision.
    let report = tend(&["report"], &session);
    assert!(
        report.contains("Guard precision: 1 flagged · 1 allowed by the user (100%"),
        "{report}"
    );

    let original: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    assert_eq!(
        tend(&["show", "o-1"], &session),
        original["tool_response"]["stdout"].as_str().unwrap()
    );

    // S2: PostToolUseFailure ignores updatedToolOutput, so a failed run is never replaced.
    assert_eq!(fs::read_to_string(dir.join("fail-out.json")).unwrap(), "");

    let why = tend(&["why", "a-1"], &session);
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
        .env("TEND_CAPTURE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = socket_of(&session);
    wait_for_daemon(&sock);

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
                        .env("TEND_SOCKET", &sock)
                        .env("TEND_HOOK_TIMEOUT_MS", "5000")
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

/// Wait until the daemon answers a ping: a socket file alone may not listen yet.
fn wait_for_daemon(sock: &Path) {
    use std::io::{BufRead, BufReader, Write};
    for _ in 0..500 {
        if let Ok(mut s) = std::os::unix::net::UnixStream::connect(sock) {
            let mut line = String::new();
            if writeln!(s, r#"{{"type":"ping"}}"#).is_ok()
                && BufReader::new(s).read_line(&mut line).is_ok()
                && line.contains("pong")
            {
                return;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon at {} did not answer", sock.display());
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
    let sock = socket_of(session);
    wait_for_daemon(&sock);
    (Daemon(child), sock)
}

fn hook(sock: &Path, event: &str, payload: &serde_json::Value) -> String {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(["hook", event])
        .env("TEND_SOCKET", sock)
        .env("TEND_HOOK_TIMEOUT_MS", "5000")
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
fn failing_test_run_is_summarized_through_tend_run() {
    let dir = tmp("wrap");
    let session = dir.join("session");
    let (daemon, sock) = start_daemon(&session, "optimize");

    let pytest_out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/outputs/pytest_fail.txt");
    // A real executable named pytest that prints a captured failing run.
    let fake = dir.join("bin/pytest");
    fs::create_dir_all(fake.parent().unwrap()).unwrap();
    fs::write(
        &fake,
        format!("#!/bin/sh\ncat '{}'\nexit 1\n", pytest_out.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let command = format!("{} -v", fake.display());
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    pre["hook_event_name"] = "PreToolUse".into();
    pre["permission_mode"] = "auto".into();
    pre["tool_input"]["command"] = command.clone().into();
    pre.as_object_mut().unwrap().remove("tool_response");

    // 1. PreToolUse: tend rewrites the command.
    let out: serde_json::Value = serde_json::from_str(&hook(&sock, "PreToolUse", &pre)).unwrap();
    let wrapped = out["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .expect("updatedInput")
        .to_string();
    // R0.4: the absolute tend binary, not whatever `tend` the agent's PATH finds.
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
        .env("TEND_SOCKET", &sock)
        .output()
        .unwrap();
    assert_eq!(ran.status.code(), Some(1), "exit code kept");
    let shown = String::from_utf8(ran.stdout).unwrap();
    assert!(shown.contains("tend show o-1"), "{shown}");
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

    let report = tend(&["report"], &session);
    assert!(report.contains("Wrapped test runs: 1"), "{report}");
    assert!(report.contains("Shaping: 1 outputs"), "{report}");
    assert!(report.contains("[applied]"), "{report}");
    assert!(!report.contains("Not shapeable"), "{report}");
    assert_eq!(
        tend(&["show", "o-1"], &session),
        fs::read_to_string(&pytest_out).unwrap(),
        "tend show gives the full original"
    );
}

/// R0.1 (D2): a stop (Claude Code's Bash timeout, Esc) prints what the command produced
/// so far and leaves no process behind.
#[test]
fn stopped_tend_run_prints_partial_output_and_leaves_no_orphans() {
    use std::io::{BufRead, BufReader, Read};
    let marker = format!("sleep 37.{}", std::process::id());
    let mut child = Command::new(bin())
        .args(["run", "--id", "t", "--sh"])
        .arg(format!("echo started; {marker}; echo never"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // `tend run` prints only when the command ends; poll for the sleep instead of
    // guessing how long the start takes (I5, T6).
    let running = || {
        let out = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        !out.stdout.is_empty()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !running() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(running(), "the command did not start");
    // SAFETY: signals the child we spawned.
    unsafe { libc_kill(child.id() as i32, 15) };
    let mut shown = String::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut shown).unwrap();
    stdout.read_to_string(&mut shown).unwrap();
    let status = child.wait().unwrap();
    assert!(shown.starts_with("started\n"), "{shown}");
    assert_eq!(status.code(), Some(128 + 15));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while running() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(), "orphan: {marker}");
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
    // tend wraps only a runner it can find: give it one, so the test does not depend on
    // a pytest on the machine's PATH (CI has none).
    let fake = dir.join("bin/pytest");
    fs::create_dir_all(fake.parent().unwrap()).unwrap();
    fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    pre["tool_input"]["command"] = format!("{} -v", fake.display()).into();
    pre.as_object_mut().unwrap().remove("tool_response");
    assert!(hook(&sock, "PreToolUse", &pre).contains("updatedInput"));

    // ~8 MiB of failing pytest output, reported by tend run.
    let big = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/outputs/pytest_fail.txt"),
    )
    .unwrap()
    .repeat(300);
    // Time hook requests straight on the socket, without a shim process to spawn, and
    // compare with the same requests on an idle daemon (I5, T6).
    let mut ls = pre.clone();
    ls["tool_input"]["command"] = "ls".into();
    ls["tool_use_id"] = "toolu_02".into();
    let direct = |sock: &Path| {
        use std::io::{BufRead, BufReader, Write};
        let t = std::time::Instant::now();
        let mut s = std::os::unix::net::UnixStream::connect(sock).unwrap();
        let req = serde_json::json!({ "type": "hook", "event": "PreToolUse", "payload": ls });
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        t.elapsed()
    };
    let baseline = (0..5).map(|_| direct(&sock)).max().unwrap();
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
    let started = std::time::Instant::now();
    let (mut slowest, mut samples) = (Duration::ZERO, 0);
    while !filter.is_finished() {
        slowest = slowest.max(direct(&sock));
        samples += 1;
        thread::sleep(Duration::from_millis(5));
    }
    let reply = filter.join().unwrap();
    assert!(
        reply.contains("tend show o-1"),
        "{}",
        &reply[..reply.len().min(200)]
    );
    assert!(samples > 0, "the filter finished before any hook was timed");
    // A hook stuck behind the filter waits for most of it (about 1.5 s in a debug build);
    // allow for a slow, shared CI runner (macOS) without hiding that.
    let filter_took = started.elapsed();
    let limit = baseline + (filter_took / 3).max(Duration::from_millis(400));
    assert!(
        slowest < limit,
        "hook waited {slowest:?} (idle {baseline:?}, filter {filter_took:?}) over {samples} samples"
    );
}

/// R0.6 (D12): the agent closed the pipe (its hook timeout); the shim still exits 0.
#[test]
fn shim_exits_zero_when_stdout_is_closed() {
    use std::io::Write;
    let dir = tmp("epipe");
    let (_daemon, sock) = start_daemon(&dir.join("session"), "optimize");
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = Command::new(bin())
        .args(["hook", "PostToolUse"])
        .env("TEND_SOCKET", &sock)
        .env("TEND_HOOK_TIMEOUT_MS", "5000")
        .stdin(std::process::Stdio::piped())
        .stdout(writer)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // A noisy pytest run in optimize mode: the shim has a summary to print.
    let payload = fs::read(fixture("post_tool_use_bash.json")).unwrap();
    child.stdin.take().unwrap().write_all(&payload).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

/// R0.8 (D6): a plain socket write cannot lift a guard or turn tend off.
#[test]
fn control_requests_need_the_session_token() {
    use std::io::{BufRead, BufReader, Write};
    let dir = tmp("token");
    let session = dir.join("session");
    fs::create_dir_all(&session).unwrap();
    fs::write(session.join("control.token"), "s3cret").unwrap();
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let raw = |req: &str| {
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line
    };
    for req in [
        r#"{"type":"allow","action":null}"#,
        r#"{"type":"set_mode","mode":"look"}"#,
        r#"{"type":"shutdown"}"#,
    ] {
        assert!(raw(req).contains("refused"), "{req}");
    }
    assert!(raw(r#"{"type":"ping"}"#).contains("pong"), "still running");
    // The CLI reads the token from the session dir.
    assert!(tend(&["allow"], &session).contains("No edit was blocked"));
    assert!(tend(&["mode", "look"], &session).contains("mode: look"));
}

/// R2.1: flag what tend got wrong; review lists it with what it points at.
#[test]
fn flags_and_review() {
    let (_dir, session) = run_session("flags", "look");
    let out = tend(
        &["flag", "a-1", "this", "summary", "hid", "the", "failure"],
        &session,
    );
    assert!(out.contains("Flagged a-1"), "{out}");
    tend(&["flag", "session", "loop", "missed"], &session);
    let bad = Command::new(bin())
        .args(["flag", "a-99", "x", "--session"])
        .arg(&session)
        .output()
        .unwrap();
    assert!(!bad.status.success(), "unknown targets are refused");

    let report = tend(&["report"], &session);
    assert!(report.contains("Flags: 2"), "{report}");
    assert!(
        report.contains("a-1: this summary hid the failure"),
        "{report}"
    );

    let home = session.parent().unwrap().parent().unwrap();
    let review = Command::new(bin())
        .arg("review")
        .env("TEND_HOME", home)
        .output()
        .unwrap();
    let review = String::from_utf8(review.stdout).unwrap();
    assert!(review.starts_with("2 flags in 1 sessions"), "{review}");
    assert!(review.contains("action a-1 (shape"), "{review}");
    assert!(
        review.contains("note: this summary hid the failure"),
        "{review}"
    );
    assert!(review.contains("look: tend why a-1 --session"), "{review}");
    assert!(review.contains("look: tend report --session"), "{review}");
}

/// R1.4: a Bash command that removes asserts from a test file gets a warning after it runs.
#[test]
fn bash_write_that_weakens_a_test_is_warned() {
    let dir = tmp("bashwrite");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("tests")).unwrap();
    let test = repo.join("tests/test_api.py");
    fs::write(
        &test,
        "def test_a():\n    assert 1 == 1\n    assert 2 == 2\n",
    )
    .unwrap();

    let command = "sed -i '/assert 2/d' tests/test_api.py";
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    pre["hook_event_name"] = "PreToolUse".into();
    pre["cwd"] = repo.display().to_string().into();
    pre["tool_input"]["command"] = command.into();
    pre.as_object_mut().unwrap().remove("tool_response");
    assert_eq!(
        hook(&sock, "PreToolUse", &pre),
        "",
        "nothing to say before the run"
    );

    let ran = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(ran.success());

    let mut post = pre.clone();
    post["hook_event_name"] = "PostToolUse".into();
    post["tool_response"] =
        serde_json::json!({ "stdout": "", "stderr": "", "interrupted": false, "isImage": false });
    let out: serde_json::Value = serde_json::from_str(&hook(&sock, "PostToolUse", &post)).unwrap();
    let context = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("a warning in context");
    assert!(context.contains("tests/test_api.py"), "{context}");
    assert!(context.contains("assertions removed (2 → 1)"), "{context}");

    let report = tend(&["report"], &session);
    assert!(report.contains("Test guard: 1"), "{report}");
}

/// R3.7: auto mode denies a wrapped call; tend says the agent may retry, and the retry
/// is not wrapped.
#[test]
fn denied_wrapped_call_may_be_retried_unwrapped() {
    let dir = tmp("denied");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let fake = dir.join("bin/pytest");
    fs::create_dir_all(fake.parent().unwrap()).unwrap();
    fs::write(&fake, "#!/bin/sh\necho ok\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    pre["hook_event_name"] = "PreToolUse".into();
    pre["permission_mode"] = "auto".into();
    pre["tool_input"]["command"] = format!("{} -q", fake.display()).into();
    pre.as_object_mut().unwrap().remove("tool_response");
    assert!(hook(&sock, "PreToolUse", &pre).contains("updatedInput"));

    let mut denied: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("permission_denied_bash.json")).unwrap())
            .unwrap();
    denied["tool_use_id"] = pre["tool_use_id"].clone();
    let out: serde_json::Value =
        serde_json::from_str(&hook(&sock, "PermissionDenied", &denied)).unwrap();
    assert_eq!(out["hookSpecificOutput"]["retry"], true);

    pre["tool_use_id"] = "toolu_02".into();
    assert_eq!(
        hook(&sock, "PreToolUse", &pre),
        "",
        "the retry runs unwrapped"
    );
    let report = tend(&["report"], &session);
    assert!(report.contains("denied by auto mode: 1 (100%)"), "{report}");
}

/// R3.5: `tend status` reads the session from TEND_SESSION_DIR and prints one line.
#[test]
fn status_line_shows_mode_and_counts() {
    let dir = tmp("status");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let post: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    hook(&sock, "PostToolUse", &post);
    let status = |env: Option<&Path>| {
        let mut cmd = Command::new(bin());
        cmd.arg("status").env_remove("TEND_SESSION_DIR");
        if let Some(d) = env {
            cmd.env("TEND_SESSION_DIR", d);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let line = status(Some(&session));
    assert!(
        line.starts_with("tend optimize · 1 tool call · shaped 1 output, saved "),
        "{line}"
    );
    assert_eq!(status(None), "", "nothing outside a tend session");
}

/// D9: a hook whose shim has already given up gets no response, and the ledger does
/// not claim the guard denied the edit.
#[test]
fn late_hook_is_not_recorded_as_applied() {
    use std::io::{BufRead, BufReader, Write};
    let dir = tmp("late");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("pre_tool_use_edit_test.json")).unwrap())
            .unwrap();
    let send = |deadline_ms: u64| {
        let req = serde_json::json!({
            "type": "hook", "event": "PreToolUse", "payload": payload, "deadline_ms": deadline_ms
        });
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        send(now - 1000)["output"].is_null(),
        "too late: no response"
    );
    let report = tend(&["report"], &session);
    assert!(report.contains("[would deny]"), "{report}");
    assert!(
        !send(now + 60_000)["output"].is_null(),
        "in time: the guard denies"
    );
}

/// Rule 1 (F1): a hook call tend cannot parse (another tend version wrote the settings)
/// exits 0. Exit 2 from PreToolUse would block the tool call.
#[test]
fn hook_argument_errors_exit_zero() {
    for args in [
        &["hook"][..],
        &["hook", "PreToolUse", "extra"],
        &["hook", "--bogus", "x"],
    ] {
        let mut child = Command::new(bin())
            .args(args)
            .env_remove("TEND_SOCKET")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"{}").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

/// F1: `tend run` with arguments it does not know still runs the command.
#[test]
fn tend_run_with_unknown_arguments_still_runs_the_command() {
    let out = Command::new(bin())
        .args(["run", "--new-flag", "--id", "x", "--sh", "echo ran; exit 3"])
        .env_remove("TEND_SOCKET")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

/// A daemon that accepts and never answers (stuck, or not tend at all).
fn silent_listener(dir: &Path) -> (PathBuf, thread::JoinHandle<()>) {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let sock = short_socket(&name);
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let handle = thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().take(1) {
            held.push(s);
            thread::sleep(Duration::from_secs(3));
        }
    });
    (sock, handle)
}

/// T2: the shim and `tend run` fail open against a daemon that never answers: within
/// their timeouts, with nothing printed (shim) or the raw bytes and exit code (run).
#[test]
fn shim_and_run_fail_open_on_a_silent_daemon() {
    let dir = tmp("silent");
    let (sock, _h) = silent_listener(&dir);
    let start = std::time::Instant::now();
    let mut child = Command::new(bin())
        .args(["hook", "PreToolUse"])
        .env("TEND_SOCKET", &sock)
        .env("TEND_HOOK_TIMEOUT_MS", "200")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let payload = fs::read(fixture("pre_tool_use_edit_test.json")).unwrap();
        child.stdin.take().unwrap().write_all(&payload).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );

    let dir = tmp("silent-run");
    let (sock, _h) = silent_listener(&dir);
    let start = std::time::Instant::now();
    let out = Command::new(bin())
        .args(["run", "--id", "t1", "--sh", r"printf 'a\377b'; exit 4"])
        .env("TEND_SOCKET", &sock)
        .env("TEND_FILTER_TIMEOUT_MS", "200")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(out.stdout, b"a\xffb");
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

/// F2: when tend cannot set itself up, the agent still runs, without tend's settings.
#[test]
fn agent_runs_when_tend_setup_fails() {
    let dir = tmp("setup-fails");
    fs::create_dir_all(dir.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("home")).unwrap();
    let agent = dir.join("agent.sh");
    fs::write(
        &agent,
        format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\necho \"$@\" > '{}/args.txt'\n", dir.display()),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(bin())
        .args(["claude", "-p", "hi"])
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_CLAUDE_BIN", &agent)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("without tend"), "{stderr}");
    assert_eq!(fs::read_to_string(dir.join("args.txt")).unwrap(), "-p hi\n");
}

/// F3: SIGTERM to `tend claude` stops the agent too, and the exit code says so.
#[test]
fn sigterm_to_the_launcher_stops_the_agent() {
    let dir = tmp("sigterm");
    let agent = dir.join("agent.sh");
    let pidfile = dir.join("agent.pid");
    fs::write(
        &agent,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\necho $$ > '{}'\nexec sleep 30\n",
            pidfile.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let mut launcher = Command::new(bin())
        .arg("claude")
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_CLAUDE_BIN", &agent)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut agent_pid = None;
    for _ in 0..250 {
        if let Some(p) = fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            agent_pid = Some(p);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let agent_pid = agent_pid.expect("agent started");
    unsafe { libc::kill(launcher.id() as i32, libc::SIGTERM) };
    let status = launcher.wait().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    let alive = unsafe { libc::kill(agent_pid, 0) } == 0;
    assert!(!alive, "the agent must not outlive the launcher");
}

/// F7: a daemon that stops mid-session is reported at exit.
#[test]
fn launcher_reports_a_daemon_that_stopped_early() {
    let dir = tmp("daemon-died");
    let agent = dir.join("agent.sh");
    let script = r#"import json, os, socket
d = os.environ["TEND_SESSION_DIR"]
tok = open(os.path.join(d, "control.token")).read().strip()
s = socket.socket(socket.AF_UNIX)
s.connect(os.environ["TEND_SOCKET"])
s.sendall((json.dumps({"type": "shutdown", "token": tok}) + "\n").encode())
s.recv(1000)
"#;
    fs::write(dir.join("stop.py"), script).unwrap();
    fs::write(
        &agent,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\ntend hook UserPromptSubmit < /dev/null\npython3 '{}/stop.py'\nsleep 0.3\n",
            dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(bin())
        .arg("claude")
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_CLAUDE_BIN", &agent)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("the daemon stopped during the session"),
        "{stderr}"
    );
}

/// R1: a run of the agent that starts no conversation (`claude --version`) leaves no
/// session behind.
#[test]
fn launch_without_a_conversation_leaves_no_session() {
    let dir = tmp("no-conversation");
    let agent = dir.join("agent.sh");
    fs::write(&agent, "#!/bin/sh\necho 2.1.303\n").unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(bin())
        .args(["claude", "--version"])
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_CLAUDE_BIN", &agent)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2.1.303\n");
    let left = fs::read_dir(dir.join("home/sessions")).unwrap().count();
    assert_eq!(left, 0);
}

/// F5: an output over the filter limit streams through byte for byte, with the exit
/// code, and `tend run` never asks the daemon about it.
#[test]
fn huge_output_streams_without_the_daemon() {
    tmp("huge");
    let sock = short_socket("count");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let out = Command::new(bin())
        .args([
            "run",
            "--id",
            "t1",
            "--sh",
            "head -c 12000000 /dev/zero | tr '\\0' 'x'; echo; exit 5",
        ])
        .env("TEND_SOCKET", &sock)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(out.stdout.len(), 12_000_001);
    assert!(out.stdout[..12_000_000].iter().all(|&b| b == b'x'));
    assert!(
        listener.accept().is_err(),
        "no filter request for a huge output"
    );
}

/// Rule 7: hook latency p95 under 20 ms, shim process included. Timing depends on the
/// machine, so it runs on demand and in CI's release job:
/// `cargo test --release -p tend-cli --test e2e -- --ignored shim_latency`.
#[test]
#[ignore]
fn shim_latency_budget() {
    let dir = tmp("latency");
    let session = dir.join("session");
    let (_daemon, sock) = start_daemon(&session, "optimize");
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("pre_tool_use_bash_auto.json")).unwrap())
            .unwrap();
    pre["tool_input"]["command"] = "ls -la".into();
    let post = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap(),
    )
    .unwrap();
    let mut times = Vec::new();
    for i in 0..200 {
        let (event, payload) = if i % 2 == 0 {
            ("PreToolUse", &pre)
        } else {
            ("PostToolUse", &post)
        };
        let t = std::time::Instant::now();
        hook(&sock, event, payload);
        times.push(t.elapsed());
    }
    times.sort();
    let p95 = times[times.len() * 95 / 100];
    eprintln!("shim p50 {:?} p95 {:?}", times[times.len() / 2], p95);
    assert!(p95 < Duration::from_millis(20), "p95 {p95:?}");
}

/// F6: when `tend run` is killed hard (SIGKILL), its command's children do not live on.
#[test]
fn hard_killed_tend_run_leaves_no_orphans() {
    let marker = format!("sleep 41.{}", std::process::id());
    let mut child = Command::new(bin())
        .args(["run", "--id", "t", "--sh"])
        .arg(format!("{marker} & sleep 30"))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let running = || {
        let out = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        !out.stdout.is_empty()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !running() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(running(), "the command did not start");
    child.kill().unwrap();
    child.wait().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while running() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(), "orphan after SIGKILL: {marker}");
}

/// E2 (experimental): with `experimental_audit = true`, the session ends with what it
/// changed in tests and checks, and `tend audit --session` shows it.
#[test]
fn session_audit_reports_what_the_agent_changed() {
    let dir = tmp("audit");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(
        repo.join("tests/test_a.py"),
        "def test_a():\n    assert 1 + 1 == 2\n\n\ndef test_b():\n    assert 2 * 2 == 4\n",
    )
    .unwrap();
    fs::write(repo.join("app.py"), "import os\n").unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    // Uncommitted work before the session is part of the base, not the session's doing.
    fs::write(repo.join("notes.txt"), "mine\n").unwrap();
    let agent = dir.join("agent.sh");
    fs::write(
        &agent,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
         echo '{}' | tend hook UserPromptSubmit\n\
         printf 'def test_a():\\n    assert 1 + 1 == 2\\n' > tests/test_a.py\n\
         printf 'import os  # noqa: F401\\n' > app.py\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("config.toml");
    fs::write(&config, "experimental_audit = true\n").unwrap();
    let out = Command::new(bin())
        .arg("claude")
        .current_dir(&repo)
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_CLAUDE_BIN", &agent)
        .env("TEND_CONFIG", &config)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "tend audit (experimental): 1 test file changed, 1 test removed (1 may weaken tests) · 1 suppression added"
        ),
        "{stderr}"
    );
    let session = fs::read_dir(dir.join("home/sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shown = tend(&["audit"], &session);
    assert!(
        shown.contains("! tests/test_a.py (modified): assertions 2 → 1, tests 2 → 1"),
        "{shown}"
    );
    assert!(shown.contains("noqa app.py:1 (no reason given)"), "{shown}");
    assert!(!shown.contains("notes.txt"), "{shown}");
    // The user's own git state is untouched.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        " M app.py\n M tests/test_a.py\n?? notes.txt\n"
    );
}
