//! rein-core: pure decision logic for rein.
//!
//! Rules for this crate (see CLAUDE.md):
//! - No I/O: no files, sockets, environment variables, clocks, threads, or model calls.
//!   Time comes in as an argument.
//! - Deterministic: the same events and timestamps give the same ledger entries.

/// Compile a regex once per call site.
macro_rules! re {
    ($pat:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        // Compiled once per call site, so a call inside a loop is cheap.
        #[allow(clippy::regex_creation_in_loops)]
        let re = RE.get_or_init(|| regex::Regex::new($pat).expect("valid regex"));
        re
    }};
}

pub mod broker;
pub mod capability;
pub mod controller;
pub mod event;
pub mod guard;
pub mod loops;
pub mod perception;
pub mod record;
pub mod session;
pub mod shape;
pub mod text;

pub use capability::Capabilities;
pub use controller::{Blob, Config, Controller, Decision, Permission, Response};
pub use event::{Event, Mode};
pub use record::{ActionKind, Body, Entry, SteerTrigger, LEDGER_FORMAT};
