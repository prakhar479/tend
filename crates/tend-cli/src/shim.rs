//! Hook shim. The agent runs `tend hook <Event>` with the hook JSON on stdin.
//!
//! Rules:
//! - Always exit 0. On any error or timeout, print nothing (fail open).
//! - Do no work except forwarding. Target p95 < 20 ms.
//! - Outside a tend session (no TEND_SOCKET), do nothing.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ipc::{self, Request};

const DEFAULT_TIMEOUT_MS: u64 = 250;

pub fn run(event: &str) {
    // Always drain stdin so the agent never blocks on a full pipe.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let socket = std::env::var_os("TEND_SOCKET").map(PathBuf::from);
    let timeout = std::env::var("TEND_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    if let Some(out) = forward(
        socket.as_deref(),
        event,
        &input,
        Duration::from_millis(timeout),
    ) {
        // The agent may have closed the pipe (its hook timeout). Never fail on that (R0.6).
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "{out}");
    }
}

/// Returns the text to print, or None. Never panics on bad input.
pub fn forward(
    socket: Option<&Path>,
    event: &str,
    input: &str,
    timeout: Duration,
) -> Option<String> {
    let socket = socket?;
    let payload = serde_json::from_str(input).ok()?;
    let req = Request::Hook {
        event: event.to_string(),
        payload,
        deadline_ms: Some(deadline_from(crate::paths::now_ms(), timeout)),
    };
    let reply = ipc::send(socket, &req, timeout).ok()?;
    reply.output.map(|v| v.to_string())
}

/// When the shim stops waiting. A huge `TEND_HOOK_TIMEOUT_MS` must not overflow.
fn deadline_from(now_ms: u64, timeout: Duration) -> u64 {
    now_ms.saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_timeout_does_not_overflow_the_deadline() {
        let d = deadline_from(1_700_000_000_000, Duration::from_millis(u64::MAX));
        assert_eq!(d, u64::MAX);
        assert_eq!(deadline_from(10, Duration::from_millis(250)), 260);
    }

    #[test]
    fn fails_open() {
        let t = Duration::from_millis(50);
        assert_eq!(forward(None, "PostToolUse", "{}", t), None);
        assert_eq!(
            forward(
                Some(Path::new("/nonexistent/w.sock")),
                "PostToolUse",
                "{}",
                t
            ),
            None
        );
        assert_eq!(
            forward(
                Some(Path::new("/nonexistent/w.sock")),
                "PostToolUse",
                "not json",
                t
            ),
            None
        );
    }
}
