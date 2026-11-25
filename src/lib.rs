//! oxisort: sort photos and videos into date-based folders.
//!
//! The run is a pipeline of separate phases (see `docs/02-plan.md`):
//! scan → extract (parallel) → plan (single-threaded, deterministic) →
//! execute (parallel transfers) → report. Every target name is fixed during
//! planning, so worker threads never race for a name.

#![deny(unsafe_op_in_unsafe_fn)]

// Windows support is deferred to the packaging run (plan decision Q5): the
// no-clobber primitives there need their own code and CI.
#[cfg(not(unix))]
compile_error!("oxisort currently supports macOS and Linux only");

pub mod classify;
pub mod cli;
pub mod date;
pub mod dateformat;
pub mod metadata;
pub mod model;
