use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! error_codes {
    ($( $(#[$doc:meta])* $variant:ident = $s:literal => $exit:ident, )*) => {
        /// Stable, documented error codes. Serialized as their `E_*` string form.
        ///
        /// Codes never change meaning between versions. New codes may be added.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $( $(#[$doc])* $variant, )*
        }

        impl ErrorCode {
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant,)*];

            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $s, )* }
            }

            /// Process exit code for this error's class.
            pub const fn exit_code(self) -> ExitCode {
                match self { $( Self::$variant => ExitCode::$exit, )* }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $( $s => Some(Self::$variant), )* _ => None }
            }
        }
    };
}

error_codes! {
    /// Unexpected internal failure.
    Internal = "E_INTERNAL" => Internal,
    /// Invalid arguments or missing input. ghf never prompts in automation.
    Usage = "E_USAGE" => Usage,
    /// Not inside a Git repository or worktree.
    NotARepo = "E_NOT_A_REPO" => Usage,
    /// The repository has no `.ghf` file. Run `git hf init`.
    NotInitialized = "E_NOT_INITIALIZED" => Usage,
    /// `.ghf` or user configuration is invalid.
    Config = "E_CONFIG" => Usage,
    /// The working tree has uncaptured changes; the operation could lose work.
    WorktreeDirty = "E_WORKTREE_DIRTY" => Refused,
    /// Another writer holds this worktree context's open trace.
    WriterLocked = "E_WRITER_LOCKED" => Refused,
    /// The worktree context has no open trace.
    NoOpenTrace = "E_NO_OPEN_TRACE" => Refused,
    /// The trace is in a state that does not allow this operation.
    TraceState = "E_TRACE_STATE" => Refused,
    /// The destination path already exists.
    PathExists = "E_PATH_EXISTS" => Refused,
    /// Apply stopped on conflicts; the tree is unchanged.
    ApplyConflict = "E_APPLY_CONFLICT" => Conflict,
    /// The same idempotency key was used with different arguments.
    IdempotencyConflict = "E_IDEMPOTENCY_CONFLICT" => Conflict,
    /// Capture failed (strict mode surfaces this to the harness).
    CaptureFailed = "E_CAPTURE_FAILED" => Internal,
    /// Checkpoints are not yet synced to the server.
    NotSynced = "E_NOT_SYNCED" => Sync,
    /// The server is not trusted for uploads. Run `git hf trust`.
    UntrustedServer = "E_UNTRUSTED_SERVER" => Sync,
    /// Missing or rejected credentials.
    Auth = "E_AUTH" => Sync,
    /// The server could not be reached or returned an error.
    Server = "E_SERVER" => Sync,
    /// The server identity changed since it was trusted.
    ServerMismatch = "E_SERVER_MISMATCH" => Sync,
    /// No checkpoint, trace or object matches the given reference.
    NotFound = "E_NOT_FOUND" => NotFound,
    /// A reference prefix matches more than one item.
    Ambiguous = "E_AMBIGUOUS" => Usage,
    /// Checkpoint content is missing locally and on the server (pruned or never synced).
    ObjectMissing = "E_OBJECT_MISSING" => NotFound,
    /// The commit does not exist locally.
    CommitNotFound = "E_COMMIT_NOT_FOUND" => NotFound,
    /// The commit is not reachable from the configured main ref.
    CommitNotReachable = "E_COMMIT_NOT_REACHABLE" => Refused,
    /// Command exists in the grammar but is not built yet.
    NotImplemented = "E_NOT_IMPLEMENTED" => NotImplemented,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Unknown codes from a newer peer degrade to E_INTERNAL rather than failing.
        Ok(Self::parse(&s).unwrap_or(Self::Internal))
    }
}

/// Exit codes, one per error class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    Internal = 1,
    Usage = 2,
    /// Refused to protect uncaptured work, an active writer, or trace state.
    Refused = 3,
    Conflict = 4,
    /// Server, network, trust or auth problem. Local state is intact.
    Sync = 5,
    NotFound = 6,
    NotImplemented = 64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_stable_string() {
        let json = serde_json::to_string(&ErrorCode::WriterLocked).unwrap();
        assert_eq!(json, "\"E_WRITER_LOCKED\"");
        assert_eq!(
            serde_json::from_str::<ErrorCode>(&json).unwrap(),
            ErrorCode::WriterLocked
        );
    }

    #[test]
    fn codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::ALL {
            assert!(seen.insert(c.as_str()), "duplicate {c}");
            assert_eq!(ErrorCode::parse(c.as_str()), Some(*c));
        }
    }
}
