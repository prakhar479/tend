//! Perception: turn raw events into facts.

pub mod classify;
pub mod signature;

pub use classify::{classify, CommandInfo, OutputKind, TestRunner};
pub use signature::{extract_failures, normalize_message, signature_of, Failure};
