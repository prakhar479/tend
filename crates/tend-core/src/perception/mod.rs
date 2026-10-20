//! Perception: turn raw events into facts.

pub mod classify;
pub mod shell;
pub mod signature;

pub use classify::{
    classify, code_write_targets, command_key, runner_segment, snapshot_update, test_write_targets,
    write_targets_where, writes_files, CommandInfo, OutputKind, TestRunner,
};
pub use signature::{extract_failures, normalize_message, signature_of, Failure};
