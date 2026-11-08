//! Lifecycle state: sync status of checkpoints, pending hold operations,
//! trace states, sync identity, idempotency keys and interrupted
//! operations. Each test pins one rule.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use serde_json::Value;

fn init(m: &Machine, dir: &Path, server: &Server) {
    m.hf(
        dir,
        &[
            "init",
            "--server",
            &server.url,
            "--repository",
            "acme/proj",
            "--trust",
        ],
    )
    .ok();
}

fn checkpoint(m: &Machine, dir: &Path) -> Value {
    m.hf(dir, &["checkpoint"]).ok()["checkpoint"].clone()
}

fn log_ids(m: &Machine, dir: &Path, trace: &str) -> Vec<String> {
    m.hf(dir, &["trace", "log", trace]).ok()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A checkpoint the server refused stays on this machine: local prune keeps
/// it even after its trace was pruned on the server.
#[test]
fn local_prune_keeps_refused_checkpoints() {
    let root = tempdir("refused");
    let server = Server::start(&root, &["--after-finalize", "0s", "--prune-interval", "0s"]);
    let (origin, work) = repo(&root);
    let a = connected(&root, "a", &server);
    init(&a, &work, &server);
    fs::write(work.join("f.txt"), "1").unwrap();
    let c1 = checkpoint(&a, &work);
    let trace = c1["trace_id"].as_str().unwrap().to_owned();
    a.hf(&work, &["sync"]).ok();

    // A keeps working offline; meanwhile B abandons the trace and the
    // server prunes it.
    fs::write(work.join("f.txt"), "2").unwrap();
    let c2 = checkpoint(&a, &work);
    let b = connected(&root, "b", &server);
    let wb = clone(&origin, &root.join("b"));
    init(&b, &wb, &server);
    b.hf(&wb, &["sync"]).ok();
    b.hf(&wb, &["trace", "finalize", &trace, "--abandoned"])
        .ok();
    let d = b.hf(&wb, &["prune"]).ok().clone();
    assert_eq!(d["server"]["pruned"][0], trace.as_str());

    // A's checkpoint is refused, then A prunes locally.
    let r = a.hf(&work, &["sync"]).ok().clone();
    assert_eq!(r["rejected"][0], c2["id"]);
    let d = a.hf(&work, &["prune"]).ok().clone();
    assert!(
        !d["local"]["pruned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == trace.as_str()),
        "{d}"
    );
    assert!(log_ids(&a, &work, &trace).contains(&c2["id"].as_str().unwrap().to_owned()));
    fs::write(work.join("f.txt"), "3").unwrap();
    checkpoint(&a, &work);
    let r = a.hf(&work, &["trace", "restore", c2["id"].as_str().unwrap()]);
    let wt = std::path::PathBuf::from(r.ok()["worktree"].as_str().unwrap());
    assert_eq!(fs::read_to_string(wt.join("f.txt")).unwrap(), "2");
}
