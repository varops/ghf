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

/// How a finalized trace ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its work landed in this commit.
    Commit(String),
    Abandoned,
}

/// A trace record that breaks the state rules, or a move its state does not
/// allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}

impl TraceRecord {
    /// A new open trace.
    pub fn open(
        id: String,
        name: Option<String>,
        created_at: i64,
        context_id: String,
        forked_from: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            state: TraceState::Open,
            created_at,
            context_id,
            forked_from,
            closed_at: None,
            finalized_at: None,
            final_commit: None,
            abandoned: false,
            pruned_at: None,
            holds: vec![],
        }
    }

    /// Check that the fields fit the state:
    /// - open: no close, finalize or prune fields;
    /// - closed: `closed_at`, and no finalize or prune fields;
    /// - finalized: `finalized_at` and exactly one of `final_commit` and
    ///   `abandoned`, no `pruned_at`;
    /// - pruned: `pruned_at`.
    pub fn validate(&self) -> Result<(), StateError> {
        let bad = |why: &str| {
            Err(StateError(format!(
                "trace {} is {} but {why}",
                self.id,
                self.state.as_str()
            )))
        };
        let finalize_fields =
            self.finalized_at.is_some() || self.final_commit.is_some() || self.abandoned;
        match self.state {
            TraceState::Open if self.closed_at.is_some() => bad("has closed_at"),
            TraceState::Open | TraceState::Closed if finalize_fields => {
                bad("has finalized_at, final_commit or abandoned")
            }
            TraceState::Open | TraceState::Closed | TraceState::Finalized
                if self.pruned_at.is_some() =>
            {
                bad("has pruned_at")
            }
            TraceState::Closed if self.closed_at.is_none() => bad("has no closed_at"),
            TraceState::Finalized if self.finalized_at.is_none() => bad("has no finalized_at"),
            TraceState::Finalized if self.final_commit.is_some() == self.abandoned => {
                bad("needs exactly one of final_commit and abandoned")
            }
            TraceState::Pruned if self.pruned_at.is_none() => bad("has no pruned_at"),
            _ => Ok(()),
        }
    }

    /// How the trace ended, if it was finalized (pruned traces keep this).
    pub fn outcome(&self) -> Option<Outcome> {
        self.finalized_at?;
        match (&self.final_commit, self.abandoned) {
            (Some(c), false) => Some(Outcome::Commit(c.clone())),
            (None, true) => Some(Outcome::Abandoned),
            _ => None,
        }
    }

    /// Stop writing at `at`. Returns whether anything changed: a trace
    /// that is already closed, finalized or pruned stays as it is.
    pub fn close(&mut self, at: i64) -> Result<bool, StateError> {
        if self.state != TraceState::Open {
            return Ok(false);
        }
        self.state = TraceState::Closed;
        self.closed_at = Some(at);
        self.clear_finalize();
        self.pruned_at = None;
        Ok(true)
    }

    fn clear_finalize(&mut self) {
        self.finalized_at = None;
        self.final_commit = None;
        self.abandoned = false;
    }

    /// Record how the trace ended, closing it first if it is open. Returns
    /// whether anything changed. A trace finalized (or pruned) with another
    /// outcome is an error; the first finalize wins.
    pub fn finalize(&mut self, at: i64, outcome: Outcome) -> Result<bool, StateError> {
        match self.state {
            TraceState::Open | TraceState::Closed => {}
            TraceState::Finalized | TraceState::Pruned => {
                if self.outcome() == Some(outcome) {
                    return Ok(false);
                }
                return Err(StateError(format!(
                    "trace {} is already {}",
                    self.id,
                    self.state.as_str()
                )));
            }
        }
        self.closed_at = self.closed_at.or(Some(at));
        self.state = TraceState::Finalized;
        self.finalized_at = Some(at);
        // A record pulled from an older server may carry a stray prune time.
        self.pruned_at = None;
        (self.final_commit, self.abandoned) = match outcome {
            Outcome::Commit(c) => (Some(c), false),
            Outcome::Abandoned => (None, true),
        };
        Ok(true)
    }

    /// Mark the trace pruned at `at`. Returns whether anything changed. An
    /// open trace cannot be pruned.
    pub fn prune(&mut self, at: i64) -> Result<bool, StateError> {
        match self.state {
            TraceState::Open => Err(StateError(format!(
                "trace {} is open and cannot be pruned",
                self.id
            ))),
            TraceState::Pruned => Ok(false),
            TraceState::Closed | TraceState::Finalized => {
                self.state = TraceState::Pruned;
                self.pruned_at = Some(at);
                Ok(true)
            }
        }
    }

    /// The record as it was before it was pruned: finalized if it has a
    /// valid outcome, else closed. Other records are returned unchanged.
    pub fn unpruned(&self) -> Self {
        let mut t = self.clone();
        if t.state != TraceState::Pruned {
            return t;
        }
        let pruned_at = t.pruned_at.take();
        if self.outcome().is_some() {
            t.state = TraceState::Finalized;
        } else {
            t.state = TraceState::Closed;
            t.closed_at = t.closed_at.or(pruned_at);
            t.clear_finalize();
        }
        t
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> TraceRecord {
        TraceRecord::open("tr_x".into(), None, 0, "wc_x".into(), None)
    }

    #[test]
    fn transitions_keep_records_valid() {
        let mut t = open();
        t.validate().unwrap();
        assert!(t.close(5).unwrap());
        assert_eq!((t.state, t.closed_at), (TraceState::Closed, Some(5)));
        t.validate().unwrap();
        assert!(!t.close(6).unwrap());
        assert!(t.finalize(7, Outcome::Abandoned).unwrap());
        t.validate().unwrap();
        assert_eq!(t.closed_at, Some(5));
        assert!(!t.finalize(8, Outcome::Abandoned).unwrap());
        assert!(t.finalize(8, Outcome::Commit("abc".into())).is_err());
        assert!(t.prune(9).unwrap());
        t.validate().unwrap();
        assert!(!t.prune(10).unwrap());
        assert_eq!(t.outcome(), Some(Outcome::Abandoned));
        assert!(!t.close(11).unwrap());

        // Finalizing an open trace closes it at the same time.
        let mut t = open();
        assert!(t.finalize(3, Outcome::Commit("abc".into())).unwrap());
        assert_eq!((t.closed_at, t.finalized_at), (Some(3), Some(3)));
        t.validate().unwrap();
        assert!(open().prune(1).is_err());
    }

    /// A closed record that an older server stored with a prune time
    /// becomes valid once finalized.
    #[test]
    fn transitions_drop_stray_fields() {
        let mut t = open();
        t.close(1).unwrap();
        t.pruned_at = Some(2);
        assert!(t.validate().is_err());
        t.finalize(3, Outcome::Abandoned).unwrap();
        t.validate().unwrap();
    }

    #[test]
    fn invalid_records_are_caught() {
        let mut t = open();
        t.pruned_at = Some(1);
        assert!(t.validate().is_err());
        let mut t = open();
        t.state = TraceState::Closed;
        assert!(t.validate().is_err());
        t.closed_at = Some(1);
        t.abandoned = true;
        assert!(t.validate().is_err());
        let mut t = open();
        t.finalize(1, Outcome::Abandoned).unwrap();
        t.final_commit = Some("abc".into());
        assert!(t.validate().is_err());
        let mut t = open();
        t.close(1).unwrap();
        t.state = TraceState::Pruned;
        assert!(t.validate().is_err());
    }

    #[test]
    fn unpruned_falls_back_to_what_the_fields_justify() {
        let mut t = open();
        t.close(1).unwrap();
        t.prune(2).unwrap();
        let u = t.unpruned();
        assert_eq!(
            (u.state, u.pruned_at, u.closed_at),
            (TraceState::Closed, None, Some(1))
        );
        u.validate().unwrap();
        let mut t = open();
        t.finalize(1, Outcome::Abandoned).unwrap();
        t.prune(2).unwrap();
        let u = t.unpruned();
        assert_eq!(u.state, TraceState::Finalized);
        u.validate().unwrap();
        // A bare pruned record without a close time.
        let mut t = open();
        t.state = TraceState::Pruned;
        t.pruned_at = Some(4);
        let u = t.unpruned();
        assert_eq!(u.closed_at, Some(4));
        u.validate().unwrap();
    }
}
