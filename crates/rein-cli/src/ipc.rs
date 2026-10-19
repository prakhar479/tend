//! Shim/CLI ↔ daemon protocol: one JSON line each way over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rein_core::Mode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// A hook call from the agent. `payload` is the hook's stdin JSON.
    Hook {
        event: String,
        payload: Value,
    },
    SetMode {
        mode: Mode,
    },
    /// `rein run` finished a wrapped command. The reply message is the text to print.
    Filter {
        tool_use_id: String,
        output: String,
    },
    /// Allow one blocked edit (the latest if `action` is None).
    Allow {
        action: Option<String>,
    },
    Ping,
    Shutdown,
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
