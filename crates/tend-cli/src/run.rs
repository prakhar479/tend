//! `tend run --id <tool_use_id> --sh <command>`: the wrapper tend puts around a test
//! command (PreToolUse rewrite). It runs the command, asks the daemon for a summary,
//! and prints it. Failed runs can only be shaped this way (S2).
//!
//! Rules:
//! - Keep the command's exit code.
//! - Fail open: without an answer from the daemon, print the original output byte for byte.
//! - Never outlive a stop: on SIGTERM/SIGINT/SIGHUP, stop everything the command started
//!   and print what it produced so far (R0.1).

use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ipc::{self, Request};

const DEFAULT_TIMEOUT_MS: u64 = 2000;
/// After the shell exits, wait this long for the pipe to close. A background child
/// (a server, `cmd &`) keeps it open, and Claude Code's own Bash does not wait for those.
const DRAIN_MS: u64 = 100;
/// Larger outputs go back to the agent unfiltered (Claude Code persists them anyway),
/// so the daemon never shapes a huge output.
pub const MAX_FILTER_BYTES: usize = 8 << 20;

/// Signal that asked `tend run` to stop (0 = none).
static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

/// Quote a string for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command line that replaces `command` in the agent's Bash call. `exe` is the
/// absolute tend binary: the agent's shell snapshot sets its own PATH.
pub fn wrapped_command(exe: &Path, tool_use_id: &str, command: &str) -> String {
    format!(
        "{} run --id {} --sh {}",
        sh_quote(&exe.display().to_string()),
        sh_quote(tool_use_id),
        sh_quote(command)
    )
}

fn shell() -> &'static str {
    if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    }
}

/// Output so far, and how the run ended.
struct Ran {
    output: Vec<u8>,
    code: i32,
    /// Stopped by a signal to `tend run` (timeout, Esc): print what we have, at once.
    signaled: bool,
    /// The output passed `MAX_FILTER_BYTES` and went straight to stdout; `output` is
    /// empty. Such an output is never filtered, so holding it in memory would only risk
    /// running out (I4, F5).
    streamed: bool,
}

/// Run the command with stdout and stderr in one pipe, as a terminal would interleave them.
fn execute(script: &str) -> Ran {
    run_child(script).unwrap_or_else(|e| Ran {
        output: format!("tend run: {e}\n").into_bytes(),
        code: 127,
        signaled: false,
        streamed: false,
    })
}

fn run_child(script: &str) -> std::io::Result<Ran> {
    let (mut reader, writer) = std::io::pipe()?;
    let mut child = {
        let mut cmd = Command::new(shell());
        cmd.arg("-c")
            .arg(script)
            .stdout(writer.try_clone()?)
            .stderr(writer)
            // Own process group, so a stop can take down everything the command started.
            .process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: prctl only sets a flag on the new process; it is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                // If tend run itself is killed hard (SIGKILL), the shell dies too.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        cmd.spawn()?
        // `cmd` drops here, closing our copies of the write end.
    };
    let pgid = child.id() as libc::pid_t;

    let output = Arc::new(Mutex::new((Vec::new(), false)));
    let (eof_tx, eof_rx) = mpsc::channel::<()>();
    {
        let output = Arc::clone(&output);
        thread::spawn(move || {
            let mut chunk = [0u8; 64 * 1024];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut guard = output.lock().unwrap_or_else(|e| e.into_inner());
                let (buf, streamed) = &mut *guard;
                if !*streamed && buf.len() + n > MAX_FILTER_BYTES {
                    // Too large to filter: hand over what we have, then pass the rest on.
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(buf);
                    let _ = out.flush();
                    buf.clear();
                    buf.shrink_to_fit();
                    *streamed = true;
                }
                if *streamed {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&chunk[..n]);
                    let _ = out.flush();
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            let _ = eof_tx.send(());
        });
    }
    let (exit_tx, exit_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });

    let (code, signaled) = loop {
        match exit_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(status) => break (exit_code(status?), false),
            Err(RecvTimeoutError::Timeout) => {
                let sig = SIGNAL.load(Ordering::SeqCst);
                if sig != 0 {
                    stop_group(pgid, &exit_rx);
                    break (128 + sig, true);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break (1, false),
        }
    };
    let _ = eof_rx.recv_timeout(Duration::from_millis(DRAIN_MS));
    let (output, streamed) = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(Ran {
        output,
        code,
        signaled,
        streamed,
    })
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// SIGTERM the whole group, then SIGKILL whatever is left after a grace period.
fn stop_group(pgid: libc::pid_t, exited: &Receiver<std::io::Result<ExitStatus>>) {
    // SAFETY: kill with a negative pid signals the process group we created.
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let _ = exited.recv_timeout(Duration::from_millis(500));
    // The shell may be gone while its children are not. SAFETY: as above.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

fn install_signal_handlers() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(
                sig,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

pub fn run(tool_use_id: &str, script: &str) -> i32 {
    install_signal_handlers();
    let ran = execute(script);
    let socket = std::env::var_os("TEND_SOCKET").map(PathBuf::from);
    let timeout = std::env::var("TEND_FILTER_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    // A stopped run prints what it has at once: whoever stopped it is waiting.
    let summary = socket
        .filter(|_| !ran.signaled && !ran.streamed && ran.output.len() <= MAX_FILTER_BYTES)
        .and_then(|s| {
            let req = Request::Filter {
                tool_use_id: tool_use_id.to_string(),
                output: String::from_utf8_lossy(&ran.output).into_owned(),
            };
            ipc::send(&s, &req, Duration::from_millis(timeout))
                .ok()
                .and_then(|r| r.message)
        });
    let mut stdout = std::io::stdout().lock();
    let _ = match summary {
        Some(text) => writeln!(stdout, "{text}"),
        None => stdout.write_all(&ran.output),
    };
    let _ = stdout.flush();
    ran.code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn quoting_survives_the_shell() {
        let cmd = r#"echo 'it'"'"'s' "$HOME" && pytest -k "a or b""#;
        let ran = execute(&format!("printf %s {}", sh_quote(cmd)));
        assert_eq!(ran.code, 0);
        assert_eq!(String::from_utf8(ran.output).unwrap(), cmd);
    }

    #[test]
    fn keeps_exit_code_and_merges_streams() {
        let ran = execute("echo out; echo err >&2; exit 3");
        assert_eq!(ran.code, 3);
        assert_eq!(String::from_utf8(ran.output).unwrap(), "out\nerr\n");
    }

    /// R0.1 (D1): a background child keeps the pipe open; do not wait for it.
    #[test]
    fn background_child_does_not_hold_the_run() {
        let t = Instant::now();
        let ran = execute("echo hi; (sleep 4 &); echo done");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert_eq!(String::from_utf8(ran.output).unwrap(), "hi\ndone\n");
        assert_eq!(ran.code, 0);
    }

    #[test]
    fn wrapper_text() {
        assert_eq!(
            wrapped_command(Path::new("/opt/tend bin/tend"), "toolu_01", "pytest -k 'x'"),
            r#"'/opt/tend bin/tend' run --id 'toolu_01' --sh 'pytest -k '\''x'\'''"#
        );
    }
}
