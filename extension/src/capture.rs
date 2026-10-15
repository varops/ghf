//! Checkpoint capture: scan the worktree, persist content locally, append a
//! checkpoint to the context's open trace. Acknowledges only after the local
//! store has committed.

use std::collections::BTreeMap;

use ghf_proto::{Change, ChangeStatus, CheckpointRecord, TraceRecord, TraceState};
use serde_json::json;

use crate::error::{Failure, Result};
use crate::scan::{self, ScanOptions, Snapshot};
use crate::store::TraceRow;
use crate::workspace::{FileLock, Workspace};
use crate::{err, fail};

#[derive(Debug, Default, Clone)]
pub struct Request {
    pub message: Option<String>,
    pub annotations: BTreeMap<String, String>,
    pub writer: String,
    pub takeover: bool,
    /// Name for a trace opened by this checkpoint.
    pub trace_name: Option<String>,
    /// Record a checkpoint even when nothing changed.
    pub force: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub checkpoint: CheckpointRecord,
    pub trace: TraceRecord,
    /// False when the worktree matched the latest checkpoint.
    pub created: bool,
    pub trace_created: bool,
}

pub fn scan_options(ws: &Workspace, persist: bool) -> Result<ScanOptions> {
    Ok(ScanOptions {
        persist,
        max_file_size: ws.cfg.max_file_size()?,
        context_id: Some(ws.ctx.id.clone()),
    })
}

/// Enforce the one-writer rule. Returns an error if another writer holds a
/// fresh lease on `trace`.
pub fn check_writer(ws: &Workspace, trace: &TraceRow, writer: &str, takeover: bool) -> Result<()> {
    let Some(owner) = trace.writer.as_deref() else {
        return Ok(());
    };
    if owner == writer || takeover {
        return Ok(());
    }
    let ttl = ws.cfg.writer_ttl_ms()?;
    let expires = trace.lease_at.unwrap_or(0).saturating_add(ttl);
    if ghf_proto::id::now_ms() < expires {
        return Err(err!(
            WriterLocked,
            "trace {} is being written by {owner}; use a separate worktree, wait for the lease to expire, or pass --takeover",
            trace.record.id
        )
        .with_data(json!({
            "trace": trace.record.id,
            "writer": owner,
            "lease_expires_at": expires,
        })));
    }
    Ok(())
}

pub fn checkpoint(ws: &Workspace, req: &Request) -> Result<Outcome> {
    let _lock = lock(ws)?;
    checkpoint_locked(ws, req)
}

/// Serialize captures and restores within one worktree.
pub fn lock(ws: &Workspace) -> Result<FileLock> {
    FileLock::acquire(&ws.lock_path("capture.lock"))
}

/// [`checkpoint`] for callers already holding [`lock`].
pub fn checkpoint_locked(ws: &Workspace, req: &Request) -> Result<Outcome> {
    let open = ws.store.open_trace(&ws.ctx.id)?;
    if let Some(t) = &open {
        check_writer(ws, t, &req.writer, req.takeover)?;
    }
    let snap = scan::scan(&ws.repo, &ws.store, &scan_options(ws, true)?)?;
    record(ws, open, snap, req, None)
}

/// Append a checkpoint for `snap` to the open trace (or a new one).
/// `parent_override` links the first checkpoint of a new trace to the
/// checkpoint it was forked from.
pub fn record(
    ws: &Workspace,
    open: Option<TraceRow>,
    snap: Snapshot,
    req: &Request,
    fork: Option<&CheckpointRecord>,
) -> Result<Outcome> {
    let now = ghf_proto::id::now_ms();
    let latest = match &open {
        Some(t) => ws.store.latest_checkpoint(&t.record.id)?,
        None => None,
    };
    let unchanged = latest.as_ref().is_some_and(|l| l.root == snap.root);
    if let (Some(t), Some(l)) = (&open, &latest)
        && unchanged
        && !req.force
        && req.message.is_none()
        && req.annotations.is_empty()
    {
        ws.store.set_writer(&t.record.id, Some(&req.writer), now)?;
        return Ok(Outcome {
            checkpoint: l.clone(),
            trace: t.record.clone(),
            created: false,
            trace_created: false,
        });
    }

    let tx = ws.store.db.unchecked_transaction()?;
    let (trace, trace_created) = match open {
        Some(t) => (t.record, false),
        None => {
            let t = TraceRecord {
                id: ghf_proto::id::new_id(ghf_proto::id::TRACE),
                name: req.trace_name.clone(),
                state: TraceState::Open,
                created_at: now,
                context_id: ws.ctx.id.clone(),
                forked_from: fork.map(|f| f.id.clone()),
                closed_at: None,
                finalized_at: None,
                final_commit: None,
                abandoned: false,
                pruned_at: None,
                holds: vec![],
            };
            ws.store.insert_trace(&t, true)?;
            (t, true)
        }
    };
    let parent = latest.as_ref().or(fork);
    let changes = match parent {
        Some(p) => scan::changes(&scan::diff_trees(
            &ws.store,
            Some(&p.root),
            Some(&snap.root),
        )?),
        None => changes_vs_head(ws, &snap)?,
    };
    let ck = CheckpointRecord {
        id: ghf_proto::id::new_id(ghf_proto::id::CHECKPOINT),
        trace_id: trace.id.clone(),
        seq: latest.as_ref().map_or(1, |l| l.seq + 1),
        parent: parent.map(|p| p.id.clone()),
        context_id: ws.ctx.id.clone(),
        base_rev: ws.repo.head(),
        created_at: now,
        author: req.writer.clone(),
        root: snap.root,
        files: snap.files.len() as u64,
        bytes: snap.bytes,
        message: req.message.clone(),
        annotations: req.annotations.clone(),
        changes,
        skipped: snap.skipped,
    };
    ws.store.insert_checkpoint(&ck, false)?;
    ws.store.set_writer(&trace.id, Some(&req.writer), now)?;
    tx.commit()?;
    Ok(Outcome {
        checkpoint: ck,
        trace,
        created: true,
        trace_created,
    })
}

/// Changes of a parentless checkpoint, relative to Git HEAD.
fn changes_vs_head(ws: &Workspace, snap: &Snapshot) -> Result<Vec<Change>> {
    let ignore = scan::ghfignore(&ws.repo.root);
    let mut out = Vec::new();
    for (code, path) in ws.repo.status_vs_head()? {
        if ignore.matched_path_or_any_parents(&path, false).is_ignore() {
            continue;
        }
        let status = match code {
            'A' => ChangeStatus::Added,
            'D' => ChangeStatus::Deleted,
            _ => ChangeStatus::Modified,
        };
        if status != ChangeStatus::Deleted && !snap.files.contains_key(&path) {
            continue;
        }
        out.push(Change {
            path,
            status,
            from: None,
        });
    }
    Ok(out)
}

/// Record a capture failure so `git hf status` can report it. Falls back to a
/// plain log file when the store itself is broken.
pub fn note_failure(
    ws: Option<&Workspace>,
    repo_git_dir: Option<&std::path::Path>,
    f: &Failure,
    strict: bool,
) {
    let stored = ws.is_some_and(|ws| {
        ws.store
            .record_failure(Some(&ws.ctx.id), f.code.as_str(), &f.message, strict)
            .is_ok()
    });
    if !stored && let Some(dir) = repo_git_dir {
        use std::io::Write;
        let path = dir.join("ghf").join("failures.log");
        let _ = std::fs::create_dir_all(dir.join("ghf"));
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                file,
                "{}\t{}\t{}\t{}",
                ghf_proto::id::now_ms(),
                f.code,
                if strict { "strict" } else { "best-effort" },
                f.message.replace('\n', " ")
            );
        }
    }
}

/// Parse `key=value` annotations.
pub fn parse_annotations(items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for item in items {
        let Some((k, v)) = item.split_once('=') else {
            fail!(Usage, "annotation must be key=value, got {item:?}");
        };
        if k.is_empty() {
            fail!(Usage, "annotation key is empty in {item:?}");
        }
        out.insert(k.to_owned(), v.to_owned());
    }
    Ok(out)
}
