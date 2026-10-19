//! Checkpoint and trace records shared by client and server.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceState {
    Open,
    Closed,
    Finalized,
    Pruned,
}

impl TraceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Finalized => "finalized",
            Self::Pruned => "pruned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Self::Open,
            "closed" => Self::Closed,
            "finalized" => Self::Finalized,
            "pruned" => Self::Pruned,
            _ => return None,
        })
    }

    fn rank(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closed => 1,
            Self::Finalized => 2,
            Self::Pruned => 3,
        }
    }

    /// States only move forward. Merging two views keeps the later state.
    pub fn max(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub name: String,
    /// Unix ms; `None` holds until released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub created_at: i64,
}

impl Hold {
    pub fn active(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|t| t > now)
    }
}

/// A trace: an ordered list of checkpoints for one unit of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub state: TraceState,
    pub created_at: i64,
    /// Worktree context that opened the trace.
    pub context_id: String,
    /// Checkpoint this trace was forked or restored from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_commit: Option<String>,
    #[serde(default)]
    pub abandoned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<i64>,
    #[serde(default)]
    pub holds: Vec<Hold>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Mode-only change (exec bit, file <-> symlink).
    Mode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub status: ChangeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    /// `too_large`, `non_utf8_path`, `unsafe_path`, `unreadable`, `submodule`, `special_file`.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// An immutable, complete, restorable worktree state plus provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub trace_id: String,
    /// 1-based position in the trace.
    pub seq: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub context_id: String,
    /// Git HEAD at capture, if the repository has commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_rev: Option<String>,
    pub created_at: i64,
    /// Agent or human id (the writer).
    pub author: String,
    /// Manifest root tree.
    pub root: ObjectId,
    pub files: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// Staged changes (the Git index relative to `base_rev`), when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexState>,
}

/// What was staged at capture time, relative to `base_rev`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexState {
    /// Manifest tree of staged additions and modifications (content as staged).
    pub staged: ObjectId,
    /// Paths staged for deletion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

impl CheckpointRecord {
    /// Every manifest root this checkpoint needs to be restorable.
    pub fn roots(&self) -> Vec<ObjectId> {
        let mut v = vec![self.root];
        if let Some(i) = &self.index {
            v.push(i.staged);
        }
        v
    }
}
