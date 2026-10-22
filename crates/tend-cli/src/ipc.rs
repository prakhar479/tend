//! Shim/CLI ↔ daemon protocol: one JSON line each way over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tend_core::Mode;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// A hook call from the agent. `payload` is the hook's stdin JSON.
    Hook {
        event: String,
        payload: Value,
        /// When the shim stops waiting (ms since the epoch). A reply after it reaches no
        /// one, so the daemon must not record its actions as applied (D9).
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    // Control requests carry the session's control token (see paths::read_token).
    SetMode {
        mode: Mode,
        #[serde(default)]
        token: Option<String>,
    },
    /// `tend run` finished a wrapped command. The reply message is the text to print.
    Filter {
        tool_use_id: String,
        output: String,
        /// The wrapped command's exit code (older `tend run` builds leave it out).
        #[serde(default)]
        exit_code: Option<i32>,
    },
    /// Allow one blocked edit (the latest if `action` is None).
    Allow {
        action: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    Ping,
    Shutdown {
        #[serde(default)]
        token: Option<String>,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    /// JSON for the shim to print on stdout. None = print nothing.
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

pub fn send(socket: &Path, req: &Request, timeout: Duration) -> Result<Reply> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(serde_json::from_str(&reply)?)
}
