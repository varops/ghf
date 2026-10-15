use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable, documented error codes. Serialized as their `E_*` string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Command or flag exists in the grammar but is not built yet.
    #[serde(rename = "E_NOT_IMPLEMENTED")]
    NotImplemented,
    /// Invalid arguments or missing input (never an interactive prompt).
    #[serde(rename = "E_USAGE")]
    Usage,
    /// Not inside a Git repository or worktree.
    #[serde(rename = "E_NOT_A_REPO")]
    NotARepo,
    /// The working tree has uncaptured changes; the operation could lose work.
    #[serde(rename = "E_WORKTREE_DIRTY")]
    WorktreeDirty,
    /// Another writer holds this worktree context's open trace.
    #[serde(rename = "E_WRITER_LOCKED")]
    WriterLocked,
    /// Apply stopped on conflicts; the tree is unchanged.
    #[serde(rename = "E_APPLY_CONFLICT")]
    ApplyConflict,
    /// Checkpoints are not yet synced to the server.
    #[serde(rename = "E_NOT_SYNCED")]
    NotSynced,
    /// Unexpected internal failure.
    #[serde(rename = "E_INTERNAL")]
    Internal,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotImplemented => "E_NOT_IMPLEMENTED",
            Self::Usage => "E_USAGE",
            Self::NotARepo => "E_NOT_A_REPO",
            Self::WorktreeDirty => "E_WORKTREE_DIRTY",
            Self::WriterLocked => "E_WRITER_LOCKED",
            Self::ApplyConflict => "E_APPLY_CONFLICT",
            Self::NotSynced => "E_NOT_SYNCED",
            Self::Internal => "E_INTERNAL",
        }
    }

    /// Process exit code for this error's class.
    pub const fn exit_code(self) -> ExitCode {
        match self {
            Self::Internal => ExitCode::Internal,
            Self::Usage | Self::NotARepo => ExitCode::Usage,
            Self::WorktreeDirty | Self::WriterLocked => ExitCode::Refused,
            Self::ApplyConflict => ExitCode::Conflict,
            Self::NotSynced => ExitCode::Sync,
            Self::NotImplemented => ExitCode::NotImplemented,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Exit codes, one per error class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    Internal = 1,
    Usage = 2,
    /// Refused to protect uncaptured work or an active writer.
    Refused = 3,
    Conflict = 4,
    Sync = 5,
    NotImplemented = 64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_stable_string() {
        let json = serde_json::to_string(&ErrorCode::WriterLocked).unwrap();
        assert_eq!(json, "\"E_WRITER_LOCKED\"");
        assert_eq!(ErrorCode::WriterLocked.as_str(), "E_WRITER_LOCKED");
    }
}
