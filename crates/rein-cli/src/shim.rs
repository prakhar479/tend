//! Hook shim. The agent runs `rein hook <Event>` with the hook JSON on stdin.
//!
//! Rules:
//! - Always exit 0. On any error or timeout, print nothing (fail open).
//! - Do no work except forwarding. Target p95 < 20 ms.
//! - Outside a rein session (no REIN_SOCKET), do nothing.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ipc::{self, Request};

const DEFAULT_TIMEOUT_MS: u64 = 250;

pub fn run(event: &str) {
    // Always drain stdin so the agent never blocks on a full pipe.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let socket = std::env::var_os("REIN_SOCKET").map(PathBuf::from);
    let timeout = std::env::var("REIN_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    if let Some(out) = forward(
        socket.as_deref(),
        event,
        &input,
        Duration::from_millis(timeout),
    ) {
        println!("{out}");
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
    };
    let reply = ipc::send(socket, &req, timeout).ok()?;
    reply.output.map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
