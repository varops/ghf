//! HTTP API v1 between `git-hf` and `ghf-server`.
//!
//! All repository routes live under `/v1/repos/{owner}/{name}/` and require
//! `Authorization: Bearer <token>`. Errors return [`ApiError`] as JSON.
//!
//! | Method | Path | Body | Response |
//! |---|---|---|---|
//! | GET | `/v1/info` | | [`ServerInfo`] (no auth) |
//! | POST | `objects/missing` | [`ObjectList`] | [`ObjectList`] |
//! | PUT | `objects/{id}` | zstd raw object | 204 |
//! | GET | `objects/{id}` | | zstd raw object |
//! | POST | `objects/batch` | [`frame`]d objects | 204 |
//! | POST | `objects/fetch` | [`ObjectList`] | [`frame`]d objects (missing ones omitted) |
//! | PUT | `traces/{id}` | [`TraceRecord`] | [`TraceRecord`] (merged) |
//! | PUT | `checkpoints/{id}` | [`CheckpointRecord`] | 204 |
//! | PUT | `traces/{id}/holds/{name}` | [`HoldRequest`] | [`TraceRecord`] |
//! | DELETE | `traces/{id}/holds/{name}` | | [`TraceRecord`] |
//! | GET | `changes?since=N` | | [`Changes`] |
//! | PUT | `clients/{id}/pending` | [`PendingReport`] | 204 |
//! | GET | `policy` | | [`Policy`](crate::Policy) |
//! | POST | `prune` | [`PruneRequest`] | [`PruneReport`] |

use serde::{Deserialize, Serialize};

use crate::{CheckpointRecord, ErrorCode, ObjectId, TraceRecord};

pub const API_VERSION: &str = "ghf/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_id: String,
    pub version: String,
    pub api: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectList {
    pub ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
    pub traces: Vec<TraceRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
    /// Cursor to pass as `since` next time.
    pub next: i64,
    pub more: bool,
}

/// A client's report of traces that still have unsynced checkpoints.
/// Pruning skips these.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingReport {
    pub traces: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneRequest {
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub pruned: Vec<String>,
    pub kept: Vec<KeptTrace>,
    /// Checkpoints kept from pruned traces because a retained fork depends on them.
    pub retained_checkpoints: Vec<String>,
    pub objects_deleted: u64,
    pub bytes_freed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeptTrace {
    pub id: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible_at: Option<i64>,
}

/// Append one object to a batch body: 32-byte id, 8-byte big-endian length,
/// then the zstd-compressed raw object.
pub fn frame(out: &mut Vec<u8>, id: &ObjectId, data: &[u8]) {
    out.extend_from_slice(&id.0);
    out.extend_from_slice(&(data.len() as u64).to_be_bytes());
    out.extend_from_slice(data);
}

/// Split a batch body into objects. `None` if the body is malformed.
pub fn unframe(mut buf: &[u8]) -> Option<Vec<(ObjectId, &[u8])>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 40 {
            return None;
        }
        let id = ObjectId(buf[..32].try_into().ok()?);
        let len = u64::from_be_bytes(buf[32..40].try_into().ok()?) as usize;
        let rest = &buf[40..];
        if rest.len() < len {
            return None;
        }
        out.push((id, &rest[..len]));
        buf = &rest[len..];
    }
    Some(out)
}

/// Validate an `owner/name` repository identifier.
pub fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    matches!(repo.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

#[cfg(test)]
mod tests {
    #[test]
    fn frames_round_trip() {
        let a = crate::object::hash(crate::object::Kind::Blob, b"a");
        let b = crate::object::hash(crate::object::Kind::Blob, b"b");
        let mut buf = Vec::new();
        super::frame(&mut buf, &a, b"one");
        super::frame(&mut buf, &b, b"");
        let v = super::unframe(&buf).unwrap();
        assert_eq!(v, vec![(a, &b"one"[..]), (b, &b""[..])]);
        assert!(super::unframe(&buf[..buf.len() - 1]).is_none());
        assert!(super::unframe(&buf[..20]).is_none());
    }

    #[test]
    fn repo_names() {
        assert!(super::valid_repo("org/project"));
        assert!(super::valid_repo("a.b/c-d_e"));
        assert!(!super::valid_repo("org"));
        assert!(!super::valid_repo("org/../x"));
        assert!(!super::valid_repo("../x"));
        assert!(!super::valid_repo("o/p/q"));
    }
}
