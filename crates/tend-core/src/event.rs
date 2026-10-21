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
    /// The agent switched conversations in the same process (`/clear`, `/resume`): a new
    /// transcript, and earlier failures and edits belong to another conversation (I6, R3).
    ConversationChanged {
        transcript_path: Option<String>,
        reason: String,
    },
    /// A tool call before it runs (non-edit tools).
    ToolRequested {
        tool_use_id: String,
        tool: String,
        input: Value,
        /// The connector says rewriting this input adds no permission prompt.
        #[serde(default)]
        rewrite_ok: bool,
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
    /// A file a shell command changed, seen after the command ran (R1.4). The connector
    /// compares the file with a snapshot from before the call. A missing file is "".
    FileChanged {
        tool_use_id: String,
        path: String,
        old_text: String,
        new_text: String,
    },
    /// A tool call that auto mode denied before it ran (R3.7).
    ToolDenied {
        tool_use_id: String,
        tool: String,
        reason: String,
    },
    PromptSubmitted {
        chars: usize,
    },
    TurnEnded,
    Compacted {
        trigger: String,
    },
    /// Where this session works, found once at start (F1): the worktree's top directory
    /// and the repo's common git dir. Outside git both are the working directory.
    Workspace {
        top: String,
        common: String,
    },
    /// Something another live tend session on this machine did (F1). `session` and `seq`
    /// name the entry in that session's ledger; `at_ms` is when it happened there.
    Peer {
        session: String,
        seq: u64,
        at_ms: u64,
        agent: String,
        activity: PeerActivity,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerActivity {
    /// The peer's session started (or tend first saw it), with its workspace.
    Joined {
        cwd: String,
        top: String,
        common: String,
    },
    /// The peer changed a file: an edit tool or a shell command. `path` is absolute.
    Edited { path: String },
    /// The peer ran a git command that changes its checkout (see `fleet::git_effect`).
    Git { command: String },
    /// The peer's daemon is gone.
    Left,
}
