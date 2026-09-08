//! Library surface for `cosmic-wmctl`.
//!
//! The CLI binary and the `cosmic-wmctl-config` GUI share this crate so the
//! rules file format and the Wayland session helpers stay in one place.

pub mod cli;
pub mod model;
pub mod rules;
pub mod wayland;
