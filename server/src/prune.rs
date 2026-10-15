//! Retention: prune eligible traces and collect unreferenced objects.
//!
//! A trace is pruned when its retention clock has elapsed, it has no active
//! hold, and no client reports unsynced checkpoints for it. Checkpoints that
//! a retained fork was created from are kept, so every retained checkpoint
//! stays restorable. Pruning never touches Git.

use std::collections::HashSet;
use std::time::Duration;

use ghf_proto::api::{KeptTrace, PruneReport};
use ghf_proto::retention::eligibility;
use ghf_proto::{Policy, TraceState};

use crate::store::{DbResult, Store};

/// Objects younger than this are never swept: an upload may be in flight.
/// `GHF_GC_GRACE` (e.g. `10m`) overrides the default of one hour.
pub fn grace() -> Duration {
    std::env::var("GHF_GC_GRACE")
        .ok()
        .and_then(|s| ghf_proto::retention::parse_duration_ms(&s))
        .map_or(Duration::from_secs(3600), |ms| {
            Duration::from_millis(ms as u64)
        })
}

pub fn prune_repo(
    store: &Store,
    repo: &str,
    default: &Policy,
    dry_run: bool,
) -> DbResult<PruneReport> {
    let now = ghf_proto::id::now_ms();
    let policy = store.policy(repo, default)?;
    let pending = store.pending_traces(repo)?;
    let traces = store.traces(repo)?;
    let mut report = PruneReport {
        dry_run,
        ..Default::default()
    };

    let mut doomed = Vec::new();
    for t in &traces {
        if t.state == TraceState::Pruned {
            continue;
        }
        let el = eligibility(t, now, &policy);
        if !el.eligible {
            report.kept.push(KeptTrace {
                id: t.id.clone(),
                reason: el.reason,
                eligible_at: el.eligible_at,
            });
        } else if pending.contains(&t.id) {
            report.kept.push(KeptTrace {
                id: t.id.clone(),
                reason: "a client has unsynced checkpoints".into(),
                eligible_at: el.eligible_at,
            });
        } else {
            doomed.push(t.clone());
        }
    }
    let doomed_ids: HashSet<&str> = doomed.iter().map(|t| t.id.as_str()).collect();
    let keep: Vec<String> = traces
        .iter()
        .filter(|t| t.state != TraceState::Pruned && !doomed_ids.contains(t.id.as_str()))
        .filter_map(|t| t.forked_from.clone())
        .collect();
    // Fork bases of already-pruned traces' forks must survive too; they are
    // retained rows, so only checkpoints of doomed traces are at stake here.
    let checkpoints = store.checkpoints(repo)?;
    for t in &doomed {
        for c in checkpoints
            .iter()
            .filter(|c| c.trace_id == t.id && keep.contains(&c.id))
        {
            report.retained_checkpoints.push(c.id.clone());
        }
        report.pruned.push(t.id.clone());
    }

    if !dry_run {
        let tx = store.db.unchecked_transaction()?;
        for t in &doomed {
            store.delete_checkpoints(repo, &t.id, &keep)?;
            let mut r = t.clone();
            r.state = TraceState::Pruned;
            r.pruned_at = Some(now);
            store.save_trace(repo, &r)?;
        }
        tx.commit()?;
    }

    let mut live = HashSet::new();
    for c in store.checkpoints(repo)? {
        if dry_run && doomed_ids.contains(c.trace_id.as_str()) && !keep.contains(&c.id) {
            continue;
        }
        store.mark(repo, &c.root, &mut live)?;
    }
    let (n, bytes) = store.sweep(repo, &live, grace(), dry_run)?;
    report.objects_deleted = n;
    report.bytes_freed = bytes;
    Ok(report)
}
