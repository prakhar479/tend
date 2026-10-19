//! `rein run --id <tool_use_id> --sh <command>`: the wrapper rein puts around a test
//! command (PreToolUse rewrite). It runs the command, asks the daemon for a summary,
//! and prints it. Failed runs can only be shaped this way (S2).
//!
//! Rules:
//! - Keep the command's exit code.
//! - Fail open: without an answer from the daemon, print the original output byte for byte.

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::ipc::{self, Request};

const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// Quote a string for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command line that replaces `command` in the agent's Bash call.
pub fn wrapped_command(tool_use_id: &str, command: &str) -> String {
    format!(
        "rein run --id {} --sh {}",
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

/// Run the command with stdout and stderr in one pipe, as a terminal would interleave them.
fn execute(script: &str) -> (Vec<u8>, i32) {
    let run = || -> std::io::Result<(Vec<u8>, i32)> {
        let (mut reader, writer) = std::io::pipe()?;
        let mut child = {
            let mut cmd = Command::new(shell());
            cmd.arg("-c")
                .arg(script)
                .stdout(writer.try_clone()?)
                .stderr(writer);
            cmd.spawn()?
            // `cmd` drops here, closing our copies of the write end.
        };
        let mut out = Vec::new();
        reader.read_to_end(&mut out)?;
        let status = child.wait()?;
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        Ok((out, code))
    };
    run().unwrap_or_else(|e| (format!("rein run: {e}\n").into_bytes(), 127))
}

pub fn run(tool_use_id: &str, script: &str) -> i32 {
    let (output, code) = execute(script);
    let socket = std::env::var_os("REIN_SOCKET").map(PathBuf::from);
    let timeout = std::env::var("REIN_FILTER_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let summary = socket.and_then(|s| {
        let req = Request::Filter {
            tool_use_id: tool_use_id.to_string(),
            output: String::from_utf8_lossy(&output).into_owned(),
        };
        ipc::send(&s, &req, Duration::from_millis(timeout))
            .ok()
            .and_then(|r| r.message)
    });
    let mut stdout = std::io::stdout().lock();
    let _ = match summary {
        Some(text) => writeln!(stdout, "{text}"),
        None => stdout.write_all(&output),
    };
    let _ = stdout.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_the_shell() {
        let cmd = r#"echo 'it'"'"'s' "$HOME" && pytest -k "a or b""#;
        let (out, code) = execute(&format!("printf %s {}", sh_quote(cmd)));
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), cmd);
    }

    #[test]
    fn keeps_exit_code_and_merges_streams() {
        let (out, code) = execute("echo out; echo err >&2; exit 3");
        assert_eq!(code, 3);
        assert_eq!(String::from_utf8(out).unwrap(), "out\nerr\n");
    }

    #[test]
    fn wrapper_text() {
        assert_eq!(
            wrapped_command("toolu_01", "pytest -k 'x'"),
            r#"rein run --id 'toolu_01' --sh 'pytest -k '\''x'\'''"#
        );
    }
}
