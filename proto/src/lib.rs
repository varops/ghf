//! Shared ghf protocol types.
//!
//! The JSON envelope, error codes and exit codes defined here are part of the
//! public automation API (PRD §9.10). Error codes never change meaning between
//! versions; new codes may be added.

mod envelope;
mod error;

pub use envelope::{ErrorBody, Response, SCHEMA_VERSION};
pub use error::{ErrorCode, ExitCode};
