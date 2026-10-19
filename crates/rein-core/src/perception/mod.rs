//! Perception: turn raw events into facts.

pub mod classify;
pub mod shell;
pub mod signature;

pub use classify::{
    classify, command_key, runner_segment, writes_files, CommandInfo, OutputKind, TestRunner,
};
pub use signature::{extract_failures, normalize_message, signature_of, Failure};
