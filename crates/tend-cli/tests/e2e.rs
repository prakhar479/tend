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
        .env("TEND_HOME", dir.join("home"))
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
        // Never the user's data dir (per-project memory, plugin maps).
        .env("TEND_HOME", session.parent().unwrap().join("home"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // The guard first: if the daemon never answers, the panic still stops it.
    let daemon = Daemon(child);
    let sock = socket_of(session);
    wait_for_daemon(&sock);
    (daemon, sock)
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

    // `-i.bak` works with GNU and BSD (macOS) sed alike.
    let command = "sed -i.bak '/assert 2/d' tests/test_api.py";
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

/// E7: the daemon reads `.tend.toml` from the session's directory.
#[test]
fn repo_policy_is_loaded_from_the_project() {
    let dir = tmp("policy");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join(".tend.toml"), "protected = [\"migrations/**\"]\n").unwrap();
    let (_daemon, sock) = start_daemon(&dir.join("session"), "optimize");
    let mut pre: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("pre_tool_use_edit_test.json")).unwrap())
            .unwrap();
    pre["cwd"] = repo.display().to_string().into();
    pre["tool_input"]["file_path"] = repo
        .join("migrations/0001.sql")
        .display()
        .to_string()
        .into();
    pre["tool_input"]["old_string"] = "CREATE TABLE a;".into();
    pre["tool_input"]["new_string"] = "CREATE TABLE b;".into();
    let out = hook(&sock, "PreToolUse", &pre);
    assert!(out.contains("protected by .tend.toml"), "{out}");
    assert!(out.contains("\"deny\""), "{out}");
}

/// E6: as a Claude Code plugin there is no launcher. The first hook of a conversation
/// starts its daemon; later hooks find it by session id; SessionEnd stops it.
#[test]
fn plugin_hooks_run_a_session_without_the_launcher() {
    let dir = tmp("plugin");
    let home = dir.join("home");
    let config = dir.join("config.toml");
    fs::write(&config, "plugin_mode = optimize\n").unwrap();
    let mut payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("pre_tool_use_edit_test.json")).unwrap())
            .unwrap();
    payload["session_id"] = "9b1d6f2e-0000-4000-8000-0000000000e6".into();
    let plugin_hook = |event: &str, extra_env: Option<(&str, &str)>| {
        use std::io::Write;
        let mut cmd = Command::new(bin());
        cmd.args(["hook", event, "--plugin"])
            .env("TEND_HOME", &home)
            .env("TEND_CONFIG", &config)
            .env("TEND_HOOK_TIMEOUT_MS", "2000")
            .env_remove("TEND_SOCKET")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        if let Some((k, v)) = extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        String::from_utf8(out.stdout).unwrap()
    };
    // Under the launcher (TEND_SOCKET set), plugin hooks stay out of the way.
    assert_eq!(
        plugin_hook("PreToolUse", Some(("TEND_SOCKET", "/nonexistent"))),
        ""
    );
    assert!(!home.join("by-session").exists());

    assert_eq!(plugin_hook("SessionStart", None), "");
    let out = plugin_hook("PreToolUse", None);
    assert!(
        out.contains("\"deny\""),
        "optimize mode from the config: {out}"
    );
    let session = fs::read_dir(home.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // The status line finds the session from the session id on stdin.
    let mut status = Command::new(bin())
        .arg("status")
        .env("TEND_HOME", &home)
        .env_remove("TEND_SESSION_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        status
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"session_id":"9b1d6f2e-0000-4000-8000-0000000000e6"}"#)
            .unwrap();
    }
    let line = String::from_utf8(status.wait_with_output().unwrap().stdout).unwrap();
    assert!(line.starts_with("tend optimize"), "{line}");
    plugin_hook("SessionEnd", None);
    assert!(socket_gone(&session), "SessionEnd stops the daemon");
    let report = tend(&["report"], &session);
    assert!(report.contains("Test guard: 1"), "{report}");
}

/// E9: what a session learns (a summary kind loosened after a re-open) carries over to
/// the next session in the same project.
#[test]
fn learned_shape_levels_carry_over_to_the_next_session() {
    let dir = tmp("memory");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let mut post: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("post_tool_use_bash.json")).unwrap())
            .unwrap();
    post["cwd"] = repo.display().to_string().into();
    let mut reopen = post.clone();
    reopen["tool_input"]["command"] = "tend show o-1".into();
    reopen["tool_use_id"] = "toolu_02".into();
    {
        let (_daemon, sock) = start_daemon(&dir.join("s1"), "optimize");
        hook(&sock, "PostToolUse", &post);
        hook(&sock, "PostToolUse", &reopen);
    }
    let memory: Vec<_> = fs::read_dir(dir.join("home/projects")).unwrap().collect();
    assert_eq!(memory.len(), 1);
    let (_daemon, sock) = start_daemon(&dir.join("s2"), "optimize");
    hook(&sock, "UserPromptSubmit", &post);
    let report = tend(&["report"], &dir.join("s2"));
    assert!(
        report.contains("Carried over from earlier sessions in this project"),
        "{report}"
    );
}

/// A daemon for session `id` under `home`, with `config`.
fn start_fleet_daemon(home: &Path, id: &str, config: &Path) -> (Daemon, PathBuf, PathBuf) {
    let session = home.join("sessions").join(id);
    fs::create_dir_all(&session).unwrap();
    let child = Command::new(bin())
        .args(["daemon", "--session-dir"])
        .arg(&session)
        .args(["--mode", "optimize"])
        .env("TEND_HOME", home)
        .env("TEND_CONFIG", config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon(child);
    let sock = socket_of(&session);
    wait_for_daemon(&sock);
    (daemon, sock, session)
}

/// Wait until `session`'s ledger contains `text` (the fleet watcher polls each second).
fn wait_for_ledger(session: &Path, text: &str) {
    for _ in 0..100 {
        if fs::read_to_string(session.join("ledger.jsonl")).is_ok_and(|l| l.contains(text)) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "{} never had {text}:\n{}",
        session.display(),
        fs::read_to_string(session.join("ledger.jsonl")).unwrap_or_default()
    );
}

/// F1: sessions in one checkout and in a worktree of it learn about each other's edits,
/// with causes that reach the other session's ledger; a peer that stops is noticed, and
/// the session keeps working.
#[test]
fn fleet_sessions_notice_each_other() {
    let dir = tmp("fleet");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    git(&repo, &["worktree", "add", "-q", "../wt"]);
    let wt = dir.join("wt");
    let home = dir.join("home");
    let config = dir.join("config.toml");
    fs::write(&config, "fleet = on\n").unwrap();

    let (_a, sock_a, sess_a) = start_fleet_daemon(&home, "1000000001-1", &config);
    let (_b, sock_b, sess_b) = start_fleet_daemon(&home, "1000000002-2", &config);
    let (_c, sock_c, _) = start_fleet_daemon(&home, "1000000003-3", &config);

    let load = |name: &str| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(fixture(name)).unwrap()).unwrap()
    };
    let at = |mut p: serde_json::Value, cwd: &Path, sid: &str| {
        p["cwd"] = cwd.display().to_string().into();
        p["session_id"] = sid.into();
        p
    };
    let edit = |cwd: &Path, sid: &str| {
        let mut p = at(load("post_tool_use_edit.json"), cwd, sid);
        p["tool_input"]["file_path"] = cwd.join("src/api.rs").display().to_string().into();
        p
    };
    for (sock, cwd, sid) in [
        (&sock_a, &repo, "a"),
        (&sock_b, &repo, "b"),
        (&sock_c, &wt, "c"),
    ] {
        hook(
            sock,
            "SessionStart",
            &at(load("session_start.json"), cwd, sid),
        );
    }
    // A edits first; then B (same checkout) and C (the worktree) edit the same file.
    wait_for_ledger(&sess_a, "\"type\":\"workspace\"");
    hook(&sock_a, "PostToolUse", &edit(&repo, "a"));
    hook(&sock_b, "PostToolUse", &edit(&repo, "b"));
    hook(&sock_c, "PostToolUse", &edit(&wt, "c"));
    wait_for_ledger(&sess_a, "1000000002-2\",\"seq\":");
    for _ in 0..100 {
        let l = fs::read_to_string(sess_a.join("ledger.jsonl")).unwrap();
        if l.matches("\"type\":\"edited\"").count() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // A's next hook carries both notices.
    let out = hook(
        &sock_a,
        "UserPromptSubmit",
        &at(load("user_prompt_submit.json"), &repo, "a"),
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect(&out);
    let context = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains("session 1000000002-2 (claude-code) edited src/api.rs after you did"),
        "{context}"
    );
    assert!(
        context.contains("Merging both branches will conflict"),
        "{context}"
    );

    // `tend why` follows the cause into B's ledger.
    let why = Command::new(bin())
        .args(["why", "a-1", "--session"])
        .arg(&sess_a)
        .env("TEND_HOME", &home)
        .output()
        .unwrap();
    let why = String::from_utf8(why.stdout).unwrap();
    assert!(why.contains("action a-1 (fleet, applied)"), "{why}");
    assert!(why.contains("In session 1000000002-2: #"), "{why}");
    assert!(why.contains("src/api.rs"), "{why}");

    // The fleet view lists all three, in one repo.
    let fleet = Command::new(bin())
        .arg("fleet")
        .env("TEND_HOME", &home)
        .output()
        .unwrap();
    let fleet = String::from_utf8(fleet.stdout).unwrap();
    assert!(fleet.starts_with("3 live tend sessions"), "{fleet}");
    assert!(fleet.contains(&wt.display().to_string()), "{fleet}");

    // B stops: A records it, and A's hooks still work.
    {
        use std::io::{BufRead, BufReader, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&sock_b).unwrap();
        writeln!(s, r#"{{"type":"shutdown"}}"#).unwrap();
        let mut reply = String::new();
        BufReader::new(s).read_line(&mut reply).unwrap();
        assert!(reply.contains("bye"), "{reply}");
    }
    wait_for_ledger(&sess_a, "\"type\":\"left\"");
    let out = hook(&sock_a, "PostToolUse", &edit(&repo, "a"));
    assert!(out.is_empty() || out.starts_with('{'), "{out}");
    let report = tend(&["report"], &sess_a);
    assert!(
        report.contains(
            "Fleet: 2 other tend sessions (1 in this checkout, 1 in other worktrees, 0 elsewhere)"
        ),
        "{report}"
    );
    assert!(report.contains("2 fleet notices"), "{report}");
    let _ = sess_b;
}

/// A fake stream-json agent: answers each user line with a result, a control request
/// with a control response; records its arguments; ends at end of input.
fn fake_stream_agent(dir: &Path) -> PathBuf {
    let script = dir.join("fake-stream-claude.sh");
    let body = format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
         echo \"$@\" > '{d}/args-'\"$TEND_FLEET_SLOT\"'.txt'\n\
         pwd > '{d}/cwd-'\"$TEND_FLEET_SLOT\"'.txt'\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n\
         while IFS= read -r line; do\n\
           case \"$line\" in\n\
             *control_request*) echo '{{\"type\":\"control_response\"}}' ;;\n\
             *) echo '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\"}}' ;;\n\
           esac\n\
         done\n",
        d = dir.display()
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn tend_in(dir: &Path, home: &Path, agent: &Path, args: &[&str]) -> String {
    let out = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .env("TEND_HOME", home)
        .env("TEND_CLAUDE_BIN", agent)
        .env("TEND_CONFIG", dir.join("no-config.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tend {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// F3a: a fleet of two stream workers in their own worktrees; one is killed and comes
/// back in the same worktree; `down` stops both; `prune` keeps the worktree with work.
#[test]
fn fleet_hub_starts_restarts_and_stops_sessions() {
    let dir = tmp("hub");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    };
    git(&["init", "-q"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    fs::write(
        repo.join("fleet.toml"),
        "preset = \"peers\"\n\n[role.peer]\nworkspace = \"worktree-per-worker\"\n",
    )
    .unwrap();
    let home = dir.join("home");
    let agent = fake_stream_agent(&dir);
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);

    /// Takes the fleet down even if the test fails, so no hub outlives it.
    struct Down<'a>(&'a Path, &'a Path, &'a Path);
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            let _ = Command::new(bin())
                .args(["fleet", "down"])
                .current_dir(self.0)
                .env("TEND_HOME", self.1)
                .env("TEND_CLAUDE_BIN", self.2)
                .output();
        }
    }
    let _down = Down(&repo, &home, &agent);

    let up = tend(&["fleet", "up", "fleet.toml"]);
    assert!(up.starts_with("fleet ") && up.contains(" is up."), "{up}");
    let status_until = |want: &dyn Fn(&str) -> bool| -> String {
        for _ in 0..150 {
            let s = tend(&["fleet", "status"]);
            if want(&s) {
                return s;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("status never matched:\n{}", tend(&["fleet", "status"]));
    };
    let running = |s: &str, slot: &str| {
        s.lines()
            .any(|l| l.trim_start().starts_with(slot) && l.contains("running"))
    };
    let s = status_until(&|s| running(s, "peer-1") && running(s, "peer-2"));
    // Each session runs headless in stream mode, under its own worktree and session id.
    let wait_file = |name: &str| {
        for _ in 0..100 {
            if let Ok(t) = fs::read_to_string(dir.join(name)) {
                if !t.is_empty() {
                    return t;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("{name} never appeared");
    };
    let args = wait_file("args-peer-1.txt");
    for want in [
        "--input-format stream-json",
        "--output-format stream-json",
        "--session-id ",
        "--append-system-prompt ",
    ] {
        assert!(args.contains(want), "{want}: {args}");
    }
    let cwd1 = wait_file("cwd-peer-1.txt");
    let cwd2 = wait_file("cwd-peer-2.txt");
    assert!(cwd1.trim().ends_with("/worktrees/peer-1"), "{cwd1}");
    assert_ne!(cwd1, cwd2);

    // Kill peer-1's session: it comes back after its backoff, in the same worktree.
    let pid: i32 = s
        .lines()
        .find(|l| l.trim_start().starts_with("peer-1"))
        .and_then(|l| l.split("pid ").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .expect(&s);
    fs::remove_file(dir.join("cwd-peer-1.txt")).unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let s = status_until(&|s| s.contains("restarts 1"));
    assert!(s.contains("peer-1") && s.contains("waiting"), "{s}");
    let s = status_until(&|s| running(s, "peer-1"));
    assert!(s.contains("restarts 1"), "{s}");
    assert_eq!(wait_file("cwd-peer-1.txt"), cwd1, "the same worktree");

    // Work left in peer-1's worktree; then the fleet goes down.
    fs::write(Path::new(cwd1.trim()).join("work.txt"), "unsaved\n").unwrap();
    let down = tend(&["fleet", "down"]);
    assert!(down.contains("The fleet is down."), "{down}");
    let s = tend(&["fleet", "status"]);
    assert!(s.contains("hub not running"), "{s}");
    assert_eq!(s.matches("stopped").count(), 2, "{s}");

    // Prune lists, then removes only the clean worktree.
    let dry = tend(&["fleet", "prune"]);
    assert!(
        dry.contains("would remove") && dry.contains("peer-2"),
        "{dry}"
    );
    assert!(dry.contains("keep") && dry.contains("uncommitted"), "{dry}");
    tend(&["fleet", "prune", "--yes"]);
    assert!(
        Path::new(cwd1.trim()).join("work.txt").exists(),
        "work is never lost"
    );
    assert!(!Path::new(cwd2.trim()).exists(), "the clean one is gone");

    // The ledger replays to the same records.
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fleet_dir.join("meta.json")).unwrap()).unwrap();
    let spec: tend_core::fleet::spec::Spec = serde_json::from_value(meta["spec"].clone()).unwrap();
    let records: Vec<tend_core::fleet::model::FleetRecord> =
        fs::read_to_string(fleet_dir.join("ledger.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let mut m = tend_core::fleet::model::Model::new(spec);
    let mut again = Vec::new();
    for r in &records {
        if let tend_core::fleet::model::FleetBody::Event { event } = &r.body {
            again.extend(m.step(event.clone(), r.ts_ms));
        }
    }
    assert_eq!(again, records);
}

/// A fake agent that acts on what it is told, through `tend task`: a lead that plans
/// three tasks and finishes when all three are reported done, and workers that finish
/// the task they get. Logs each worker's task with the time it finished.
fn fake_acting_agent(dir: &Path) -> PathBuf {
    let script = dir.join("fake-acting-claude.sh");
    let body = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.304 (Claude Code)' && exit 0
D='{d}'
result() {{ echo '{{"type":"result","subtype":"success","result":"ok"}}'; }}
while IFS= read -r line; do
  case "$line" in
    *LEAD-PLAN*)
      tend task add "A" >> "$D/lead.txt" 2>&1
      tend task add "B" --depends t-1.1 >> "$D/lead.txt" 2>&1
      tend task add "C" --role worker >> "$D/lead.txt" 2>&1
      tend fleet down >> "$D/lead.txt" 2>&1
      result ;;
    *"is done ("*)
      echo x >> "$D/reports"
      if [ "$(wc -l < "$D/reports")" -ge 3 ]; then
        tend task done t-1 "all three done" >> "$D/lead.txt" 2>&1
      fi
      result ;;
    *"tend task done t-"*)
      id=$(printf '%s' "$line" | sed -n 's/.*tend task done \(t-[0-9.]*\).*/\1/p')
      sleep 0.3
      echo "$TEND_FLEET_SLOT $id" >> "$D/work.log"
      tend task done "$id" "ok from $TEND_FLEET_SLOT" >> "$D/done.txt" 2>&1
      result ;;
    *) result ;;
  esac
done
"#,
        d = dir.display()
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// F5a: the lead adds three tasks, two oneshot workers do them, and the dependent one
/// starts only after its dependency is done. Sessions are told apart by their process
/// tree: the lead's own `tend fleet down` is refused.
#[test]
fn fleet_lead_plans_tasks_and_workers_do_them_in_order() {
    let dir = tmp("tasks");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    };
    git(&["init", "-q"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    fs::write(
        repo.join("fleet.toml"),
        "preset = \"lead\"\n\n[role.worker]\nreplicas = \"0..2\"\n\n[limits]\nsessions = 3\n",
    )
    .unwrap();
    let home = dir.join("home");
    let agent = fake_acting_agent(&dir);
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    struct Down<'a>(&'a Path, &'a Path, &'a Path);
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            let _ = Command::new(bin())
                .args(["fleet", "down"])
                .current_dir(self.0)
                .env("TEND_HOME", self.1)
                .env("TEND_CLAUDE_BIN", self.2)
                .output();
        }
    }
    let _down = Down(&repo, &home, &agent);

    tend(&["fleet", "up", "fleet.toml"]);
    let added = tend(&["task", "add", "LEAD-PLAN", "--role", "lead"]);
    assert_eq!(added.trim(), "Added t-1, for lead.");
    let mut board = String::new();
    for _ in 0..200 {
        board = tend(&["task", "list"]);
        if board.lines().filter(|l| l.contains(" done ")).count() == 4 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    for id in ["t-1 ", "t-1.1 ", "t-1.2 ", "t-1.3 "] {
        assert!(
            board
                .lines()
                .any(|l| l.trim_start().starts_with(id) && l.contains(" done ")),
            "{id}: {board}"
        );
    }
    // The lead's tasks are parts of its own (t-1.N); B waited for A.
    let lead = fs::read_to_string(dir.join("lead.txt")).unwrap();
    assert!(lead.contains("Added t-1.1, for worker."), "{lead}");
    assert!(lead.contains("Added t-1.2, for worker."), "{lead}");
    assert!(
        lead.contains("Fleet control is the user's"),
        "the lead cannot take the fleet down: {lead}"
    );
    let work = fs::read_to_string(dir.join("work.log")).unwrap();
    let order: Vec<&str> = work.lines().map(|l| l.split(' ').nth(1).unwrap()).collect();
    let pos = |t: &str| order.iter().position(|x| *x == t).expect(&work);
    assert!(pos("t-1.1") < pos("t-1.2"), "B after A: {work}");
    // Each task ran in its own fresh worker; at most two at once.
    let slots: Vec<&str> = work.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(slots.len(), 3);
    assert!(slots.iter().all(|s| s.starts_with("worker-")), "{work}");
    let done = fs::read_to_string(dir.join("done.txt")).unwrap();
    assert_eq!(done.matches("is done.").count(), 3, "{done}");
    // The user hears that the lead's task is done.
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let inbox = fs::read_to_string(fleet_dir.join("inbox.jsonl")).unwrap();
    assert!(
        inbox.contains("t-1 is done (lead-1): all three done"),
        "{inbox}"
    );
    // A user cannot finish a worker's task by pretending to be it: the hub knows the
    // caller is not in that session's process tree, so it acts as the user, who may.
    let out = Command::new(bin())
        .args(["task", "done", "t-9", "x"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .env("TEND_FLEET_SLOT", "worker-1")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("There is no task t-9"));
}

/// A git repo with `files` committed and `fleet.toml`; returns (dir, repo, home).
fn fleet_repo(name: &str, files: &[(&str, &str)], fleet: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = tmp(name);
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    for (path, text) in files {
        let p = repo.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fs::write(repo.join("fleet.toml"), fleet).unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-qm", "base"],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    }
    let home = dir.join("home");
    (dir, repo, home)
}

fn fake_script(dir: &Path, name: &str, cases: &str) -> PathBuf {
    let script = dir.join(name);
    let body = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.304 (Claude Code)' && exit 0
D='{d}'
result() {{ echo '{{"type":"result","subtype":"success","result":"ok"}}'; }}
task_id() {{ printf '%s' "$1" | sed -n 's/.*tend task done \(t-[0-9.]*\).*/\1/p'; }}
while IFS= read -r line; do
  case "$line" in
{cases}
    *) result ;;
  esac
done
"#,
        d = dir.display()
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Take a fleet down when dropped, so a failing test leaves no hub behind.
struct FleetDown(PathBuf, PathBuf, PathBuf);
impl Drop for FleetDown {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .args(["fleet", "down"])
            .current_dir(&self.0)
            .env("TEND_HOME", &self.1)
            .env("TEND_CLAUDE_BIN", &self.2)
            .output();
    }
}

fn board_until(tend: &dyn Fn(&[&str]) -> String, want: &dyn Fn(&str) -> bool) -> String {
    for _ in 0..200 {
        let b = tend(&["task", "list"]);
        if want(&b) {
            return b;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("board never matched:\n{}", tend(&["task", "list"]));
}

/// F5b: a worker that says done while the check fails gets the failure back, in the
/// same session; its fix passes the check, and only then is the task done.
#[test]
fn fleet_check_gate_sends_a_failure_back_to_the_worker() {
    let check =
        "test -f ok.txt || { echo 'FAILED tests/test_x.py::test_ok - ok.txt is missing'; exit 1; }";
    let (dir, repo, home) = fleet_repo(
        "gate",
        &[("a.txt", "a\n")],
        &format!(
            "preset = \"lead\"\ncheck = \"{}\"\n",
            check.replace('"', "\\\"")
        ),
    );
    let agent = fake_script(
        &dir,
        "fake-gate.sh",
        r#"    *"Attempt 2 of this task"*)
      id=$(task_id "$line"); echo "$line" > "$D/second.txt"
      touch ok.txt; echo "$TEND_FLEET_SLOT fixed $id" >> "$D/log"
      tend task done "$id" "fixed it" >> "$D/done.txt" 2>&1; result ;;
    *"tend task done t-"*)
      id=$(task_id "$line"); echo "$TEND_FLEET_SLOT first $id" >> "$D/log"
      tend task done "$id" "first try" >> "$D/done.txt" 2>&1; result ;;"#,
    );
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    tend(&["task", "add", "make ok.txt", "--role", "worker"]);
    let board = board_until(&tend, &|b| b.contains(" done "));
    assert!(
        board.contains("1 failed attempt") && board.contains("\"fixed it\""),
        "{board}"
    );
    // The same session got its own failure back, with the check's words.
    let log = fs::read_to_string(dir.join("log")).unwrap();
    assert_eq!(log, "worker-1 first t-1\nworker-1 fixed t-1\n");
    let second = fs::read_to_string(dir.join("second.txt")).unwrap();
    assert!(
        second.contains("failed (exit 1)") && second.contains("ok.txt is missing"),
        "{second}"
    );
    let done = fs::read_to_string(dir.join("done.txt")).unwrap();
    assert_eq!(done.matches("tend runs its check").count(), 2, "{done}");
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let inbox = fs::read_to_string(fleet_dir.join("inbox.jsonl")).unwrap();
    assert!(inbox.contains("passed: fixed it"), "{inbox}");
    // The report, and the merge plan (F5e).
    let report = tend(&["fleet", "report"]);
    assert!(
        report.contains("Tasks: 1 · done 1") && report.contains("attempts that did not count: 1"),
        "{report}"
    );
    assert!(
        report.contains("Merge plan (1 worktree with changes)")
            && report.contains("worker-1 (t-1 done)  ok.txt"),
        "{report}"
    );
    let refused = Command::new(bin())
        .args(["fleet", "merge", "--yes"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("take it down"),
        "only when the fleet is down"
    );
    tend(&["fleet", "down"]);
    // git as a fresh machine has it: an identity, no signing.
    let merged = Command::new(bin())
        .args(["fleet", "merge", "--yes"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "user.name")
        .env("GIT_CONFIG_VALUE_1", "t")
        .env("GIT_CONFIG_KEY_2", "user.email")
        .env("GIT_CONFIG_VALUE_2", "t@t")
        .output()
        .unwrap();
    let merged = String::from_utf8(merged.stdout).unwrap();
    assert!(
        merged.contains("committed worker-1's changes on tend/")
            && merged.contains("git merge tend/"),
        "{merged}"
    );
    let wt = fleet_dir.join("worktrees/worker-1");
    let log = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "t-1 done");
    // The user's branch did not move.
    let head = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).lines().count(), 1);
}

/// F5b with the audit on (experimental): a change that deletes a test passes its check,
/// but waits for its creator, who accepts it.
#[test]
fn fleet_check_gate_holds_a_flagged_change_for_review() {
    let test =
        "def test_one():\n    assert 1 + 1 == 2\n\n\ndef test_two():\n    assert 2 * 2 == 4\n";
    let (dir, repo, home) = fleet_repo(
        "review",
        &[("tests/test_math.py", test)],
        "preset = \"lead\"\ncheck = \"true\"\n",
    );
    fs::write(dir.join("config.toml"), "experimental_audit = true\n").unwrap();
    let agent = fake_script(
        &dir,
        "fake-review.sh",
        r#"    *"tend task done t-"*)
      id=$(task_id "$line")
      printf 'def test_one():\n    assert 1 + 1 == 2\n' > tests/test_math.py
      tend task done "$id" "simplified the tests" >> "$D/done.txt" 2>&1; result ;;"#,
    );
    let tend = |args: &[&str]| {
        let out = Command::new(bin())
            .args(args)
            .current_dir(&repo)
            .env("TEND_HOME", &home)
            .env("TEND_CLAUDE_BIN", &agent)
            .env("TEND_CONFIG", dir.join("config.toml"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    tend(&["task", "add", "tidy the tests", "--role", "worker"]);
    let board = board_until(&tend, &|b| b.contains(" review "));
    assert!(board.contains("t-1"), "{board}");
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let inbox = fs::read_to_string(fleet_dir.join("inbox.jsonl")).unwrap();
    assert!(
        inbox.contains("audit (experimental) flags its diff: tests/test_math.py"),
        "{inbox}"
    );
    // The user created it, so the user decides.
    let ok = tend(&["task", "done", "t-1", "fine: test_two was a duplicate"]);
    assert_eq!(ok.trim(), "t-1 is done.");
    board_until(&tend, &|b| b.contains(" done "));
}

/// F5c: a fleet session's daemon denies an edit outside its role's scope, and one into
/// what another session in the same worktree claimed; claims are read again on change.
#[test]
fn fleet_scopes_and_claims_deny_edits() {
    let dir = tmp("scopes");
    let session = dir.join("session");
    fs::create_dir_all(&session).unwrap();
    let claims = dir.join("claims.json");
    fs::write(&claims, "[]").unwrap();
    let rules = serde_json::json!({
        "root": "/home/dev/shop", "slot": "implementer-1",
        "may_not_write": ["tests/**"], "claims": claims.display().to_string(),
    });
    let child = Command::new(bin())
        .args(["daemon", "--session-dir"])
        .arg(&session)
        .args(["--mode", "optimize"])
        .env("TEND_HOME", dir.join("home"))
        .env("TEND_FLEET_SLOT", "implementer-1")
        .env("TEND_FLEET_RULES", rules.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _daemon = Daemon(child);
    let sock = socket_of(&session);
    wait_for_daemon(&sock);
    let edit = |path: &str| {
        let mut p: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(fixture("pre_tool_use_edit_test.json")).unwrap(),
        )
        .unwrap();
        p["tool_input"]["file_path"] = path.into();
        p["tool_input"]["old_string"] = "x = 1".into();
        p["tool_input"]["new_string"] = "x = 2".into();
        hook(&sock, "PreToolUse", &p)
    };
    let out = edit("/home/dev/shop/tests/test_new.py");
    assert!(
        out.contains("\"deny\"") && out.contains("may not write `tests/**`"),
        "{out}"
    );
    assert_eq!(edit("/home/dev/shop/src/api.py"), "", "in scope, unclaimed");
    // Another session in this worktree claims src/** for its task.
    thread::sleep(Duration::from_millis(20));
    fs::write(
        &claims,
        r#"[{"slot":"implementer-2","task":"t-3","worktree":"/home/dev/shop","scope":["src/**"]}]"#,
    )
    .unwrap();
    let out = edit("/home/dev/shop/src/api.py");
    assert!(
        out.contains("implementer-2 claimed `src/**` for t-3"),
        "{out}"
    );
}

/// F5g: a tree. The lead splits its task between two sub-leads; each splits its part
/// between workers of its own team; reports go to whoever gave the task, up the tree.
#[test]
fn fleet_tree_splits_work_down_and_reports_up() {
    let (dir, repo, home) = fleet_repo(
        "tree",
        &[("a.txt", "a\n")],
        "preset = \"tree\"\n\n[limits]\nsessions = 7\n",
    );
    let agent = fake_script(
        &dir,
        "fake-tree.sh",
        r#"    *TREE-PLAN*)
      task_id "$line" > "$D/task-$TEND_FLEET_SLOT"
      tend task add "SUB-WORK one" --role sub >> "$D/plan.txt" 2>&1
      tend task add "SUB-WORK two" --role sub >> "$D/plan.txt" 2>&1
      result ;;
    *SUB-WORK*)
      task_id "$line" > "$D/task-$TEND_FLEET_SLOT"
      tend task add "LEAF a" >> "$D/plan.txt" 2>&1
      tend task add "LEAF b" >> "$D/plan.txt" 2>&1
      result ;;
    *"is done ("*)
      echo "$line" | grep -o 't-[0-9.]* is done ([^)]*)' >> "$D/heard-$TEND_FLEET_SLOT"
      if [ "$(wc -l < "$D/heard-$TEND_FLEET_SLOT")" -ge 2 ]; then
        tend task done "$(cat "$D/task-$TEND_FLEET_SLOT")" "both parts done" >> "$D/plan.txt" 2>&1
      fi
      result ;;
    *"tend task done t-"*)
      id=$(task_id "$line"); echo "$TEND_FLEET_SLOT $id" >> "$D/work.log"
      tend task done "$id" "leaf done" >> "$D/plan.txt" 2>&1; result ;;"#,
    );
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    tend(&["task", "add", "TREE-PLAN", "--role", "lead"]);
    let board = board_until(&tend, &|b| {
        b.lines().filter(|l| l.contains(" done ")).count() == 7
    });
    for id in [
        "t-1 ", "t-1.1 ", "t-1.2 ", "t-1.1.1 ", "t-1.1.2 ", "t-1.2.1 ", "t-1.2.2 ",
    ] {
        assert!(
            board
                .lines()
                .any(|l| l.trim_start().starts_with(id) && l.contains(" done ")),
            "{id}\n{board}"
        );
    }
    // Leaves ran in their sub-lead's team; each sub-lead heard from its own workers.
    let work = fs::read_to_string(dir.join("work.log")).unwrap();
    for line in work.lines() {
        let (slot, task) = line.split_once(' ').unwrap();
        let team = if task.starts_with("t-1.1.") {
            "sub-1/"
        } else {
            "sub-2/"
        };
        assert!(slot.starts_with(team), "{line}");
    }
    let heard = fs::read_to_string(dir.join("heard-sub-1")).unwrap();
    assert!(
        heard
            .lines()
            .all(|l| l.starts_with("t-1.1.") && l.contains("(sub-1/worker-")),
        "{heard}"
    );
    let heard = fs::read_to_string(dir.join("heard-lead-1")).unwrap();
    assert!(
        heard.contains("t-1.1 is done (sub-1)") && heard.contains("t-1.2 is done (sub-2)"),
        "{heard}"
    );
    // G7: the ledger replays to the same records; another file decides differently.
    let same = tend(&["fleet", "replay"]);
    assert!(same.contains("replay to the same"), "{same}");
    fs::write(
        repo.join("tight.toml"),
        "preset = \"tree\"\n\n[limits]\nsessions = 3\n",
    )
    .unwrap();
    let other = tend(&["fleet", "replay", "--file", "tight.toml"]);
    assert!(
        other.contains("Commands, as recorded and with tight.toml") && other.contains('*'),
        "{other}"
    );
}

/// F4: a session in the user's terminal joins a fleet, asks the headless lead a
/// question with `tend msg`, and gets the answer as added context at its next hook,
/// labelled as another session's words. When it exits, its slot ends for good.
#[test]
fn fleet_messages_reach_a_terminal_session_at_its_next_hook() {
    let (dir, repo, home) = fleet_repo(
        "msg",
        &[("a.txt", "a\n")],
        "preset = \"lead\"\n\n[role.reviewer]\nrun = \"attached\"\n",
    );
    let lead = fake_script(
        &dir,
        "fake-lead.sh",
        r#"    *"question from rev"*)
      tend msg rev "the answer is 42" >> "$D/lead.txt" 2>&1; result ;;"#,
    );
    let terminal = dir.join("fake-terminal.sh");
    fs::write(
        &terminal,
        format!(
            r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.304 (Claude Code)' && exit 0
D='{d}'
tend hook SessionStart < '{start}' > /dev/null
tend msg @lead "what is the answer?" > "$D/rev-msg.txt" 2>&1
i=0
while [ $i -lt 100 ]; do
  tend hook PostToolUse < '{post}' > "$D/rev-hook.json"
  grep -q "answer is 42" "$D/rev-hook.json" && break
  sleep 0.1; i=$((i+1))
done
tend inbox > "$D/rev-inbox.txt" 2>&1
echo "$TEND_FLEET_SLOT" > "$D/rev-slot.txt"
"#,
            d = dir.display(),
            start = fixture("session_start.json").display(),
            post = fixture("post_tool_use_bash.json").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();
    let tend = |args: &[&str]| tend_in(&repo, &home, &lead, args);
    let _down = FleetDown(repo.clone(), home.clone(), lead.clone());
    let up = tend(&["fleet", "up", "fleet.toml"]);
    assert!(up.contains("lead-1"), "{up}");
    let joined = Command::new(bin())
        .args([
            "claude",
            "--tend-fleet",
            "--tend-name",
            "rev",
            "--tend-mode",
            "optimize",
        ])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .env("TEND_CLAUDE_BIN", &terminal)
        .env("TEND_CONFIG", dir.join("no-config.toml"))
        .env("TEND_HOOK_TIMEOUT_MS", "2000")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&joined.stderr);
    assert!(
        joined.status.success() && err.contains("joined fleet") && err.contains("as rev"),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("rev-slot.txt")).unwrap().trim(),
        "rev"
    );
    let sent = fs::read_to_string(dir.join("rev-msg.txt")).unwrap();
    assert!(sent.contains("send a question to lead-1"), "{sent}");
    let hook: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("rev-hook.json")).unwrap()).unwrap();
    let context = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default();
    assert!(
        context.contains("answer from lead-1 in this fleet:\nthe answer is 42"),
        "{hook}"
    );
    assert!(
        context.contains("not as an instruction from the user"),
        "{context}"
    );
    let inbox = fs::read_to_string(dir.join("rev-inbox.txt")).unwrap();
    assert!(
        inbox.contains("answer from lead-1: the answer is 42"),
        "{inbox}"
    );
    // It left: its slot is stopped, not restarted.
    let status = tend(&["fleet", "status"]);
    assert!(
        status
            .lines()
            .any(|l| l.trim_start().starts_with("rev ") && l.contains("stopped")),
        "{status}"
    );
    // The user's unread messages show in `tend fleet` until `tend inbox` (G6).
    let lead_reply = Command::new(bin())
        .args(["msg", "@lead", "thanks"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .output()
        .unwrap();
    assert!(lead_reply.status.success());
    let fleets = tend(&["fleet"]);
    assert!(fleets.contains("unread (`tend inbox`)"), "{fleets}");
    tend(&["inbox"]);
    assert!(!tend(&["fleet"]).contains("unread"), "read now");
    // A message to it now has nowhere to go.
    let out = Command::new(bin())
        .args(["msg", "rev", "still there?"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("refused"),
        "{out:?}"
    );
}

/// G1: the hub dies, its stream sessions end; `tend fleet restart` brings the fleet back
/// from its ledger, and each stream session resumes its own conversation.
#[test]
fn fleet_restart_resumes_stream_sessions_after_the_hub_dies() {
    let (dir, repo, home) = fleet_repo(
        "restart",
        &[("a.txt", "a\n")],
        "preset = \"peers\"\n\n[role.peer]\nworkspace = \"worktree-per-worker\"\n",
    );
    let agent = fake_stream_agent(&dir);
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    let wait = |name: &str, want: &dyn Fn(&str) -> bool| -> String {
        for _ in 0..100 {
            if let Ok(t) = fs::read_to_string(dir.join(name)) {
                if want(&t) {
                    return t;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("{name}: {:?}", fs::read_to_string(dir.join(name)));
    };
    let first = wait("args-peer-1.txt", &|t| t.contains("--session-id "));
    let session = first
        .split("--session-id ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let pid: i32 = fs::read_to_string(fleet_dir.join("hub.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    for _ in 0..100 {
        if tend(&["fleet", "status"]).contains("hub not running") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let restarted = tend(&["fleet", "restart"]);
    assert!(restarted.contains(" · up · "), "{restarted}");
    let again = wait("args-peer-1.txt", &|t| t.contains("--resume "));
    assert!(again.contains(&format!("--resume {session}")), "{again}");
    let s = tend(&["fleet", "status"]);
    assert!(
        s.lines().filter(|l| l.contains("running")).count() == 2,
        "{s}"
    );
    assert!(!s.contains("restarts"), "a restart is not a crash: {s}");
}

/// Review R: `tend fleet down` lets go of a session in the user's terminal; it never
/// signals it.
#[test]
fn fleet_down_leaves_a_terminal_session_running() {
    let (dir, repo, home) = fleet_repo(
        "detach",
        &[("a.txt", "a\n")],
        "preset = \"lead\"\n\n[role.reviewer]\nrun = \"attached\"\n",
    );
    let lead = fake_stream_agent(&dir);
    let terminal = dir.join("fake-terminal.sh");
    fs::write(
        &terminal,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
             touch '{d}/joined'\n\
             i=0; while [ ! -f '{d}/go' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n\
             echo alive > '{d}/alive'\n",
            d = dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();
    let tend = |args: &[&str]| tend_in(&repo, &home, &lead, args);
    let _down = FleetDown(repo.clone(), home.clone(), lead.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    let mut child = Command::new(bin())
        .args(["claude", "--tend-fleet", "--tend-name", "rev"])
        .current_dir(&repo)
        .env("TEND_HOME", &home)
        .env("TEND_CLAUDE_BIN", &terminal)
        .env("TEND_CONFIG", dir.join("no-config.toml"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if dir.join("joined").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let down = tend(&["fleet", "down"]);
    assert!(down.contains("The fleet is down."), "{down}");
    fs::write(dir.join("go"), "").unwrap();
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "the terminal session was not killed: {status:?}"
    );
    assert!(dir.join("alive").exists());
    let fleet_dir = fs::read_dir(home.join("fleets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let inbox = fs::read_to_string(fleet_dir.join("inbox.jsonl")).unwrap();
    assert!(
        inbox.contains("rev left the fleet; its terminal session goes on"),
        "{inbox}"
    );
}

/// Bench finding: a task that depends on others starts in a fresh worktree, so the hub
/// brings in its dependencies' work (a changed file and a new one) before delivering it.
#[test]
fn fleet_brings_dependencies_work_into_a_fresh_worktree() {
    let (dir, repo, home) = fleet_repo("deps", &[("a.txt", "a\n")], "preset = \"pool\"\n");
    let agent = fake_script(
        &dir,
        "fake-deps.sh",
        r#"    *"Task t-1:"*)
      echo changed > a.txt; echo new > part_a.txt
      tend task done t-1 "made A" >> "$D/done.txt" 2>&1; result ;;
    *"Task t-2:"*)
      { test -f part_a.txt && grep -q changed a.txt && echo saw-a || echo missing-a; } > "$D/b-saw.txt"
      case "$line" in *"brought the work of t-1 (worker-1)"*) echo told > "$D/b-told.txt" ;; esac
      tend task done t-2 "made B" >> "$D/done.txt" 2>&1; result ;;"#,
    );
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    tend(&["task", "add", "A"]);
    tend(&["task", "add", "B", "--depends", "t-1"]);
    board_until(&tend, &|b| b.matches(" done ").count() == 2);
    assert_eq!(
        fs::read_to_string(dir.join("b-saw.txt")).unwrap().trim(),
        "saw-a"
    );
    assert!(
        dir.join("b-told.txt").exists(),
        "the agent is told what came in"
    );
}

/// Bench finding: a part that is done brings its work up into the worktree of the
/// session that holds the parent task, so the lead can check the whole.
#[test]
fn fleet_brings_a_done_parts_work_up_to_its_lead() {
    let (dir, repo, home) = fleet_repo("lift", &[("a.txt", "a\n")], "preset = \"lead\"\n");
    let agent = fake_script(
        &dir,
        "fake-lift.sh",
        r#"    *PLAN*)
      tend task add "PART one" >> "$D/lead.txt" 2>&1; result ;;
    *"is done ("*)
      sleep 0.2
      { test -f part.txt && echo lifted || echo missing; } > "$D/lead-saw.txt"
      tend task done t-1 "whole done" >> "$D/lead.txt" 2>&1; result ;;
    *"Task t-1.1:"*)
      echo part > part.txt
      tend task done t-1.1 "part done" >> "$D/done.txt" 2>&1; result ;;"#,
    );
    let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
    let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
    tend(&["fleet", "up", "fleet.toml"]);
    tend(&["task", "add", "PLAN", "--role", "lead"]);
    board_until(&tend, &|b| b.matches(" done ").count() == 2);
    assert_eq!(
        fs::read_to_string(dir.join("lead-saw.txt")).unwrap().trim(),
        "lifted"
    );
}

/// F2: session A fails a test run, edits, and passes it: a learned fix in the machine's
/// store. Session B in the same project hits the same failure twice: the second time
/// it is told how A fixed it.
#[test]
fn a_fix_learned_in_one_session_is_recalled_in_another() {
    let dir = tmp("learn");
    let home = dir.join("home");
    let config = dir.join("config.toml");
    fs::write(&config, "learn = on\nwrap_tests = false\n").unwrap();
    let load = |name: &str| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(fixture(name)).unwrap()).unwrap()
    };
    let start = load("session_start.json");
    let failure = load("post_tool_use_failure_bash.json");
    let mut edit = load("post_tool_use_edit.json");
    edit["tool_input"]["file_path"] = "/home/dev/shop/shop/orders.py".into();
    let mut pass = load("post_tool_use_bash.json");
    pass["tool_input"]["command"] = failure["tool_input"]["command"].clone();
    pass["tool_response"]["stdout"] = "tests/test_api.py ..                [100%]\n\n============================== 2 passed in 0.02s ===============================\n".into();

    let (_a, sock_a, sess_a) = start_fleet_daemon(&home, "1000000001-1", &config);
    hook(&sock_a, "SessionStart", &start);
    hook(&sock_a, "PostToolUseFailure", &failure);
    hook(&sock_a, "PostToolUse", &edit);
    hook(&sock_a, "PostToolUse", &pass);
    let fixes = fs::read_to_string(home.join("learn/fixes.jsonl")).unwrap_or_default();
    assert!(
        fixes.contains("\"session\":\"1000000001-1\"") && fixes.contains("shop/orders.py"),
        "{fixes}"
    );
    let report = tend(&["report", "--full"], &sess_a);
    assert!(
        report.contains("Learned fixes:") && report.contains("fixed by changes to shop/orders.py"),
        "{report}"
    );

    let (_b, sock_b, _) = start_fleet_daemon(&home, "1000000002-2", &config);
    hook(&sock_b, "SessionStart", &start);
    let first = hook(&sock_b, "PostToolUseFailure", &failure);
    assert!(
        !first.contains("fixed before"),
        "the first hit says nothing: {first}"
    );
    let second = hook(&sock_b, "PostToolUseFailure", &failure);
    assert!(
        second.contains("this failure was fixed before, in this project")
            && second.contains("shop/orders.py"),
        "{second}"
    );
}

/// F2 notes: the user's note is kept, an agent's waits for review; approved notes reach
/// a new session of the project at its start.
#[test]
fn project_notes_reach_new_sessions_once_approved() {
    let dir = tmp("notes");
    let project = dir.join("proj");
    fs::create_dir_all(&project).unwrap();
    let home = dir.join("home");
    let config = dir.join("config.toml");
    fs::write(&config, "learn = on\n").unwrap();
    let run = |args: &[&str], agent: bool| {
        let mut c = Command::new(bin());
        c.args(args)
            .current_dir(&project)
            .env("TEND_HOME", &home)
            .env_remove("TEND_SESSION_DIR");
        if agent {
            c.env("TEND_SESSION_DIR", dir.join("sessions/agent-1"));
        }
        let o = c.output().unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    };
    // Without a terminal, even outside a session, a note waits (an agent can clear
    // TEND_SESSION_DIR, but its shell has no terminal).
    assert!(
        run(&["note", "integration tests need DB_URL"], false).contains("for the user's review")
    );
    assert!(run(&["note", "the cache dir is ./tmp"], true).contains("for the user's review"));
    let list = run(&["notes"], false);
    assert!(
        list.contains("n-1  pending") && list.contains("n-2  pending"),
        "{list}"
    );
    // An agent cannot approve one.
    let refused = Command::new(bin())
        .args(["notes", "approve", "n-2"])
        .current_dir(&project)
        .env("TEND_HOME", &home)
        .env("TEND_SESSION_DIR", dir.join("sessions/agent-1"))
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(run(&["notes"], false).contains("n-2  pending"));
    run(&["notes", "approve", "n-1"], false);
    run(&["notes", "approve", "n-2"], false);

    let (_d, sock, _) = start_fleet_daemon(&home, "1000000001-1", &config);
    let mut start: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture("session_start.json")).unwrap()).unwrap();
    start["cwd"] = project.display().to_string().into();
    let out = hook(&sock, "SessionStart", &start);
    assert!(
        out.contains("notes for this project, kept by the user")
            && out.contains("DB_URL")
            && out.contains("./tmp"),
        "{out}"
    );
}

/// M1: two parts change adjacent lines of one file (a patch would not apply: each one's
/// context is the other's change); the dependent task's worktree gets both, merged.
/// When they change the same line, it gets the conflict and is told where.
#[test]
fn fleet_merges_parts_work_three_ways() {
    for (second, want_conflict) in [("3s/.*/C/", false), ("1s/.*/Z/", true)] {
        let (dir, repo, home) = fleet_repo(
            if want_conflict {
                "merge-conflict"
            } else {
                "merge-clean"
            },
            &[("f.txt", "a\nb\nc\n")],
            "preset = \"pool\"\n",
        );
        let agent = fake_script(
            &dir,
            "fake-merge.sh",
            &format!(
                r#"    *"Task t-1:"*)
      sed -i '1s/.*/A/' f.txt; tend task done t-1 one >> "$D/done.txt" 2>&1; result ;;
    *"Task t-2:"*)
      sed -i '{second}' f.txt; tend task done t-2 two >> "$D/done.txt" 2>&1; result ;;
    *"Task t-3:"*)
      cp f.txt "$D/seen.txt"; echo "$line" > "$D/told.txt"
      tend task done t-3 three >> "$D/done.txt" 2>&1; result ;;"#
            ),
        );
        let tend = |args: &[&str]| tend_in(&repo, &home, &agent, args);
        let _down = FleetDown(repo.clone(), home.clone(), agent.clone());
        tend(&["fleet", "up", "fleet.toml"]);
        tend(&["task", "add", "one"]);
        tend(&["task", "add", "two"]);
        board_until(&tend, &|b| b.matches(" done ").count() == 2);
        tend(&["task", "add", "three", "--depends", "t-1,t-2"]);
        board_until(&tend, &|b| b.matches(" done ").count() == 3);
        let seen = fs::read_to_string(dir.join("seen.txt")).unwrap();
        let told = fs::read_to_string(dir.join("told.txt")).unwrap();
        if want_conflict {
            assert!(
                seen.contains("<<<<<<<") && seen.contains('Z') && seen.contains('A'),
                "{seen}"
            );
            assert!(told.contains("conflicted in f.txt"), "{told}");
        } else {
            assert_eq!(seen, "A\nb\nC\n");
            assert!(
                told.contains("brought the work of t-1 (worker-1), t-2 (worker-2)"),
                "{told}"
            );
        }
    }
}
