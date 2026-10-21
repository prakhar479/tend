//! Fleets: tend sessions on one machine that know about each other (F1) and run as a
//! team the user describes (F3b). Design: `docs/fleet-2026-10-21.md` and
//! `docs/fleet-topologies-2026-10-21.md`.

mod awareness;
pub mod model;
pub mod scope;
pub mod spec;

pub use awareness::*;
