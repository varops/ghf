//! `ghf watch`: checkpoint after file changes that no harness hook sees
//! (background processes, generators, a human in an editor).
//!
//! The watcher is a companion to whoever holds the trace: it records changes
//! under the current writer's identity, marked `captured_by=watch`, and never
//! claims or refreshes the writer lease. With no writer, it writes as itself.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ghf_proto::Response;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::json;

use crate::capture::{self, Request};
use crate::error::{Failure, Result};
use crate::remote;
use crate::workspace::Workspace;
use crate::{err, fail};

pub struct Options {
    /// Quiet period after the last change before a checkpoint.
    pub debounce: Duration,
    /// Upper bound on how long continuous changes can delay a checkpoint.
    pub max_wait: Duration,
    pub json: bool,
}

/// Paths whose changes never warrant a checkpoint: Git's and ghf's own state.
fn relevant(root: &Path, git_dir: &Path, common_dir: &Path, path: &Path) -> bool {
    if path.starts_with(git_dir) || path.starts_with(common_dir) {
        return false;
    }
    match path.strip_prefix(root) {
        Ok(rel) => !rel.components().any(|c| c.as_os_str() == ".git"),
        Err(_) => false,
    }
}

fn writer() -> String {
    if let Ok(w) = std::env::var("GHF_WRITER")
        && !w.is_empty()
    {
        return w;
    }
    let user = crate::platform::user_name();
    format!("watch:{user}")
}

fn report(json: bool, display: &str, result: std::result::Result<capture::Outcome, Failure>) {
    match result {
        Ok(o) if o.created => {
            let c = &o.checkpoint;
            if json {
                let resp = Response::ok(
                    "watch",
                    json!({"created": true, "checkpoint": c, "trace": o.trace}),
                );
                println!("{}", serde_json::to_string(&resp).expect("serializes"));
            } else {
                println!(
                    "checkpoint {} (trace {}, #{}, {} change(s), writer {})",
                    c.id,
                    c.trace_id,
                    c.seq,
                    c.changes.len(),
                    c.author
                );
            }
        }
        Ok(_) => {}
        Err(f) => {
            if json {
                let resp = Response::err("watch", f.code, &f.message, f.data);
                println!("{}", serde_json::to_string(&resp).expect("serializes"));
            } else {
                eprintln!("{display} watch: {} ({})", f.message, f.code);
            }
        }
    }
}

/// Run until interrupted. Returns only on a setup error.
pub fn run(ws: &Workspace, opts: &Options, display: &str) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)
        .map_err(|e| err!(Internal, "cannot start the file watcher: {e}"))?;
    watcher
        .watch(&ws.repo.root, RecursiveMode::Recursive)
        .map_err(|e| err!(Internal, "cannot watch {}: {e}", ws.repo.root.display()))?;
    if !opts.json {
        eprintln!(
            "{display} watch: watching {} (checkpoint {} ms after the last change; Ctrl-C to stop)",
            ws.repo.root.display(),
            opts.debounce.as_millis()
        );
    }
    let root =
        crate::platform::canonicalize(&ws.repo.root).unwrap_or_else(|_| ws.repo.root.clone());
    let git_dir =
        crate::platform::canonicalize(&ws.repo.git_dir).unwrap_or_else(|_| ws.repo.git_dir.clone());
    let common = crate::platform::canonicalize(&ws.repo.common_dir)
        .unwrap_or_else(|_| ws.repo.common_dir.clone());
    let req = Request {
        writer: writer(),
        annotations: BTreeMap::from([("captured_by".into(), "watch".into())]),
        companion: true,
        only_if_changed: true,
        ..Default::default()
    };

    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    loop {
        let event = match first {
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            Some(start) => {
                let due = (last + opts.debounce).min(start + opts.max_wait);
                rx.recv_timeout(due.saturating_duration_since(Instant::now()))
            }
        };
        match event {
            Ok(Ok(ev)) => {
                let interesting = !matches!(ev.kind, EventKind::Access(_))
                    && ev
                        .paths
                        .iter()
                        .any(|p| relevant(&root, &git_dir, &common, p));
                if interesting {
                    last = Instant::now();
                    first.get_or_insert(last);
                }
            }
            Ok(Err(e)) => eprintln!("{display} watch: watcher error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                first = None;
                let result = capture::checkpoint(ws, &req);
                match &result {
                    Ok(o) if o.created => remote::spawn_background(ws),
                    Err(f) => capture::note_failure(Some(ws), Some(&ws.repo.git_dir), f, false),
                    _ => {}
                }
                report(opts.json, display, result);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                fail!(Internal, "the file watcher stopped unexpectedly")
            }
        }
    }
}
