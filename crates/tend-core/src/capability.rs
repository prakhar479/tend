//! What each agent lets tend do. The controller plans only with declared capabilities.
//! Values marked [Verify] are not yet confirmed by a Phase 0 spike (docs/TASKS.md).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub replace_output: bool,
    /// Replace the output of a failed tool call (Claude Code: PostToolUseFailure).
    pub replace_failed_output: bool,
    pub rewrite_input: bool,
    pub deny: bool,
    pub ask: bool,
    pub inject_context: bool,
    pub continue_turn: bool,
    pub block_compaction: bool,
    /// Tell the agent it may retry a call that auto mode denied (Claude Code:
    /// PermissionDenied `retry`, in the 2.1.303 hook docs). [Verify] live (R3.7).
    pub retry_denied: bool,
    /// Largest hook message the agent passes to the model, if it has a limit.
    pub hook_output_limit_tokens: Option<usize>,
}

impl Capabilities {
    /// Claude Code. `replace_output` uses PostToolUse `updatedToolOutput` in the shape of
    /// `tool_response` (S2). PostToolUseFailure ignores `updatedToolOutput` but passes
    /// `additionalContext` (S2), so failed runs can be steered but not shaped.
    /// `ask` is false: the interactive prompt shows neither user nor agent the reason (S6),
    /// so the guard downgrades to deny, which the agent sees with its reason.
    pub const CLAUDE_CODE: Self = Self {
        replace_output: true,
        replace_failed_output: false,
        rewrite_input: true,
        deny: true,
        ask: false,
        inject_context: true,
        continue_turn: true,
        block_compaction: true,
        retry_denied: true,
        hook_output_limit_tokens: None,
    };

    /// Codex. Output replacement works only through PostToolUse `decision: "block"`.
    /// `ask` is parsed but not supported.
    /// [Verify] whether a failed call can be block-replaced (S10).
    pub const CODEX: Self = Self {
        replace_output: true,
        replace_failed_output: false,
        rewrite_input: true,
        deny: true,
        ask: false,
        inject_context: true,
        continue_turn: true,
        block_compaction: true,
        retry_denied: false,
        hook_output_limit_tokens: Some(2500),
    };

    /// Observe only (retro analysis of transcripts).
    pub const OBSERVE_ONLY: Self = Self {
        replace_output: false,
        replace_failed_output: false,
        rewrite_input: false,
        deny: false,
        ask: false,
        inject_context: false,
        continue_turn: false,
        block_compaction: false,
        retry_denied: false,
        hook_output_limit_tokens: None,
    };
}
