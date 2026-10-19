//! Common event vocabulary. Connectors translate agent traffic into these events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Observe and record. Never change the session.
    Look,
    /// Apply actions the agent supports.
    Optimize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First event of a session. The controller fills `mode` when it records the event.
    SessionStarted {
        agent: String,
        cwd: String,
        transcript_path: Option<String>,
        #[serde(default)]
        mode: Option<Mode>,
    },
    /// A tool call before it runs (non-edit tools).
    ToolRequested {
        tool_use_id: String,
        tool: String,
        input: Value,
    },
    /// An edit the agent wants to make. Sent before the edit runs.
    EditProposed {
        tool_use_id: String,
        path: String,
        old_text: String,
        new_text: String,
    },
    /// A tool call after it ran. `output` is what the agent would see.
    ToolCompleted {
        tool_use_id: String,
        tool: String,
        input: Value,
        output: String,
        failed: bool,
    },
    /// An edit that happened. `after_hash` is the file content hash after the edit.
    FileEdited {
        tool_use_id: String,
        path: String,
        after_hash: Option<String>,
    },
    PromptSubmitted {
        chars: usize,
    },
    TurnEnded,
    Compacted {
        trigger: String,
    },
}
