//! Resolve user-supplied checkpoint and trace references.
//!
//! - `@` is the latest checkpoint of this worktree's current trace; `@~N` is N before it.
//! - `tr_…` (or a unique prefix) names a trace; as a checkpoint it means the trace's latest.
//! - `ck_…`, or a unique prefix with or without `ck_`, names a checkpoint.

use ghf_proto::CheckpointRecord;

use crate::error::Result;
use crate::store::TraceRow;
use crate::workspace::Workspace;
use crate::{err, fail};

/// The trace commands default to: the open trace of this worktree context,
/// else its most recent trace.
pub fn current_trace(ws: &Workspace) -> Result<Option<TraceRow>> {
    match ws.store.open_trace(&ws.ctx.id)? {
        Some(t) => Ok(Some(t)),
        None => ws.store.latest_trace(&ws.ctx.id),
    }
}

pub fn trace(ws: &Workspace, r: Option<&str>) -> Result<TraceRow> {
    let Some(r) = r else {
        return current_trace(ws)?
            .ok_or_else(|| err!(NoOpenTrace, "this worktree has no trace yet"));
    };
    let prefix = if r.starts_with("tr_") {
        r.to_owned()
    } else {
        format!("tr_{r}")
    };
    let ids = ws.store.trace_ids_with_prefix(&prefix)?;
    match ids.as_slice() {
        [id] => Ok(ws.store.trace(id)?.expect("trace exists")),
        [] => fail!(NotFound, "no trace matches {r:?}"),
        _ => fail!(Ambiguous, "{r:?} matches more than one trace"),
    }
}

pub fn checkpoint(ws: &Workspace, r: &str) -> Result<CheckpointRecord> {
    if let Some(rest) = r.strip_prefix('@') {
        let back: i64 = match rest {
            "" => 0,
            n => n
                .strip_prefix('~')
                .and_then(|n| {
                    if n.is_empty() {
                        Some(1)
                    } else {
                        n.parse().ok()
                    }
                })
                .ok_or_else(|| err!(Usage, "invalid reference {r:?}; use @ or @~N"))?,
        };
        let t = current_trace(ws)?
            .ok_or_else(|| err!(NoOpenTrace, "this worktree has no trace yet"))?;
        let latest = ws
            .store
            .latest_checkpoint(&t.record.id)?
            .ok_or_else(|| err!(NotFound, "trace {} has no checkpoints", t.record.id))?;
        let seq = latest.seq - back;
        return ws
            .store
            .checkpoints(&t.record.id)?
            .into_iter()
            .find(|c| c.seq == seq)
            .ok_or_else(|| err!(NotFound, "{r} is before the start of trace {}", t.record.id));
    }
    if r.starts_with("tr_") {
        let t = trace(ws, Some(r))?;
        return ws
            .store
            .latest_checkpoint(&t.record.id)?
            .ok_or_else(|| err!(NotFound, "trace {} has no checkpoints", t.record.id));
    }
    let prefix = if r.starts_with("ck_") {
        r.to_owned()
    } else {
        format!("ck_{r}")
    };
    if prefix.len() < 7 {
        fail!(
            Usage,
            "checkpoint prefix {r:?} is too short; use at least 4 characters"
        );
    }
    let ids = ws.store.checkpoint_ids_with_prefix(&prefix)?;
    match ids.as_slice() {
        [id] => Ok(ws.store.checkpoint(id)?.expect("checkpoint exists")),
        [] => fail!(NotFound, "no checkpoint matches {r:?}"),
        _ => fail!(Ambiguous, "{r:?} matches more than one checkpoint"),
    }
}
