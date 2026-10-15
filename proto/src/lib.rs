//! Shared ghf protocol types.
//!
//! The JSON envelope, error codes and exit codes are part of the public
//! automation API (PRD §9.10). The object format, records and HTTP API types
//! are shared by `git-hf` and `ghf-server`.

pub mod api;
mod envelope;
mod error;
pub mod id;
pub mod object;
mod record;
pub mod retention;

pub use envelope::{ErrorBody, Response, SCHEMA_VERSION};
pub use error::{ErrorCode, ExitCode};
pub use object::ObjectId;
pub use record::{Change, ChangeStatus, CheckpointRecord, Hold, Skipped, TraceRecord, TraceState};
pub use retention::Policy;
