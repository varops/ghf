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

/// A hold the server refuses for good (its trace is pruned there) is dropped
/// and reported instead of failing every later sync; a hold on a trace this
/// machine knows is pruned is refused up front.
#[test]
fn refused_hold_ops_do_not_block_sync() {
    let root = tempdir("holds");
    let server = Server::start(&root, &["--prune-interval", "0s"]);
    let (_, work) = repo(&root);
    let a = connected(&root, "a", &server);
    init(&a, &work, &server);
    fs::write(work.join("f.txt"), "1").unwrap();
    let trace = checkpoint(&a, &work)["trace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    a.hf(&work, &["trace", "finalize", "--abandoned"]).ok();
    a.hf(&work, &["sync"]).ok();
    server.admin(&["prune", "--after-finalize", "0s"]);

    // This machine has not seen the prune yet, so it queues the hold.
    a.hf(&work, &["trace", "pin", &trace]).ok();
    let r = a.hf(&work, &["sync"]).ok().clone();
    assert_eq!(r["refused_holds"][0]["trace"], trace.as_str(), "{r}");
    assert_eq!(r["refused_holds"][0]["code"], "E_TRACE_STATE");
    assert_eq!(r["hold_ops"], 0);
    a.hf(&work, &["sync"]).ok();
    let st = a.hf(&work, &["status"]).ok().clone();
    assert_eq!(st["refused_holds"][0]["name"], "pin", "{st}");
    assert!(
        st["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("hold"))
    );

    // Now it knows: refused locally, nothing queued.
    let r = a.hf(&work, &["trace", "pin", &trace]);
    assert_eq!(r.err_code(), "E_TRACE_STATE");
    assert_eq!(a.hf(&work, &["sync"]).ok()["hold_ops"], 0);
}

fn trace_ids(m: &Machine, dir: &Path, env: &[(&str, &str)]) -> Vec<Value> {
    m.hf_env(dir, &["trace", "list", "--all"], env).ok()["traces"]
        .as_array()
        .unwrap()
        .clone()
}

/// Sync progress belongs to one server and repository. Pointing the
/// repository at another server pulls that server's history from the start
/// and pushes everything again; the old server's policy no longer applies.
#[test]
fn switching_servers_resets_sync_progress() {
    let root = tempdir("switch");
    fs::create_dir_all(root.join("s1")).unwrap();
    fs::create_dir_all(root.join("s2")).unwrap();
    let s1 = Server::start(
        &root.join("s1"),
        &["--after-finalize", "0s", "--prune-interval", "0s"],
    );
    let s2 = Server::start(&root.join("s2"), &["--prune-interval", "0s"]);
    let (origin, work) = repo(&root);
    let a = connected(&root, "a", &s1);
    init(&a, &work, &s1);
    fs::write(work.join("f.txt"), "1").unwrap();
    let c1 = checkpoint(&a, &work);
    let t1 = c1["trace_id"].as_str().unwrap().to_owned();
    fs::write(work.join("f.txt"), "2").unwrap();
    checkpoint(&a, &work);
    a.hf(&work, &["trace", "finalize", "--abandoned"]).ok();
    a.hf(&work, &["sync"]).ok();
    let eligible = |env: &[(&str, &str)]| {
        trace_ids(&a, &work, env)
            .iter()
            .find(|t| t["id"] == t1.as_str())
            .unwrap()["retention"]["eligible"]
            .clone()
    };
    assert_eq!(eligible(&[]), true);

    // Without a server, the old server's policy does not apply.
    a.hf(&work, &["init", "--strict", "--force"]).ok();
    assert_eq!(eligible(&[]), false);

    // Another machine already works against the second server.
    let b = connected(&root, "b", &s2);
    let wb = clone(&origin, &root.join("b"));
    init(&b, &wb, &s2);
    fs::write(wb.join("g.txt"), "b").unwrap();
    let tb = checkpoint(&b, &wb)["trace_id"].as_str().unwrap().to_owned();
    b.hf(&wb, &["sync"]).ok();

    let env = [("GHF_TOKEN", s2.token.as_str())];
    a.hf_env(
        &work,
        &[
            "init",
            "--server",
            &s2.url,
            "--repository",
            "acme/proj",
            "--trust",
            "--force",
        ],
        &env,
    )
    .ok();
    let r = a.hf_env(&work, &["sync"], &env).ok().clone();
    assert_eq!(r["pushed_checkpoints"], 2, "{r}");
    assert!(
        trace_ids(&a, &work, &env)
            .iter()
            .any(|t| t["id"] == tb.as_str()),
        "the second server's traces are pulled from the start"
    );
    b.hf(&wb, &["sync"]).ok();
    assert_eq!(log_ids(&b, &wb, &t1).len(), 2);
}

/// When two machines finalize the same trace differently, the first to
/// reach the server wins; the other adopts that result and is told.
#[test]
fn a_losing_finalize_is_reported() {
    let root = tempdir("finalize-race");
    let server = Server::start(&root, &["--prune-interval", "0s"]);
    let (origin, work) = repo(&root);
    let a = connected(&root, "a", &server);
    init(&a, &work, &server);
    fs::write(work.join("f.txt"), "1").unwrap();
    let trace = checkpoint(&a, &work)["trace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    a.hf(&work, &["sync"]).ok();
    let b = connected(&root, "b", &server);
    let wb = clone(&origin, &root.join("b"));
    init(&b, &wb, &server);
    b.hf(&wb, &["sync"]).ok();

    b.hf(&wb, &["trace", "finalize", &trace, "--abandoned"])
        .ok();
    b.hf(&wb, &["sync"]).ok();
    let sha = git(&work, &["rev-parse", "HEAD"]);
    a.hf(&work, &["trace", "finalize", "--commit", &sha]).ok();
    let r = a.hf(&work, &["sync"]).ok().clone();
    let c = &r["trace_conflicts"][0];
    assert_eq!(c["trace"], trace.as_str(), "{r}");
    assert_eq!(c["ours"]["final_commit"], sha.as_str());
    assert_eq!(c["kept"]["abandoned"], true);
    let t = a.hf(&work, &["trace", "log", &trace]).ok()["trace"].clone();
    assert_eq!(t["abandoned"], true);
    assert_eq!(t["final_commit"], Value::Null);
}

fn trace_state(m: &Machine, dir: &Path, trace: &str) -> Value {
    m.hf(dir, &["trace", "log", trace]).ok()["trace"]["state"].clone()
}

/// A worktree context that is gone leaves no open trace behind: one
/// replaced because the directory moved, or one whose worktree was removed
/// (noticed by `prune`).
#[test]
fn orphaned_open_traces_are_closed() {
    let root = tempdir("orphans");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    m.hf(&work, &["init"]).ok();

    // The directory "moved": its context records another path.
    fs::write(work.join("f.txt"), "1").unwrap();
    let t1 = checkpoint(&m, &work)["trace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ctx_file = work.join(".git/ghf/context.json");
    let mut ctx: Value = serde_json::from_str(&fs::read_to_string(&ctx_file).unwrap()).unwrap();
    ctx["path"] = Value::String(root.join("elsewhere").display().to_string());
    fs::write(&ctx_file, ctx.to_string()).unwrap();
    m.hf(&work, &["status"]).ok();
    assert_eq!(trace_state(&m, &work, &t1), "closed");

    // A linked worktree with an open trace is removed.
    let wt = root.join("wt2");
    git(
        &work,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    fs::copy(work.join(".ghf"), wt.join(".ghf")).unwrap();
    fs::write(wt.join("g.txt"), "1").unwrap();
    let t2 = checkpoint(&m, &wt)["trace_id"].as_str().unwrap().to_owned();
    git(
        &work,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    assert_eq!(trace_state(&m, &work, &t2), "open");
    let d = m.hf(&work, &["prune", "--dry-run"]).ok().clone();
    assert_eq!(d["local"]["closed_orphans"][0], t2.as_str(), "{d}");
    assert_eq!(trace_state(&m, &work, &t2), "open");
    m.hf(&work, &["prune"]).ok();
    assert_eq!(trace_state(&m, &work, &t2), "closed");
    // This worktree's own trace is untouched.
    fs::write(work.join("f.txt"), "2").unwrap();
    let t3 = checkpoint(&m, &work)["trace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    m.hf(&work, &["prune"]).ok();
    assert_eq!(trace_state(&m, &work, &t3), "open");
}

fn store_db(work: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(work.join(".git/ghf/ghf.db")).unwrap()
}

/// An idempotency key covers the whole command line, even an argument
/// equal to the key itself.
#[test]
fn idempotency_fingerprint_keeps_arguments_equal_to_the_key() {
    let root = tempdir("idem-fp");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    m.hf(&work, &["init"]).ok();
    fs::write(work.join("f.txt"), "1").unwrap();
    let t = checkpoint(&m, &work)["trace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    m.hf(&work, &["trace", "close", &t, "--idempotency-key", &t])
        .ok();
    let r = m.hf(&work, &["trace", "close", "--idempotency-key", &t]);
    assert_eq!(r.err_code(), "E_IDEMPOTENCY_CONFLICT", "{}", r.stdout);
}

/// A failed run is recorded too: replaying its key returns the same
/// failure instead of acting again.
#[test]
fn idempotency_replays_failures() {
    let root = tempdir("idem-fail");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    m.hf(&work, &["init"]).ok();
    fs::write(work.join("f.txt"), "1").unwrap();
    checkpoint(&m, &work);
    fs::write(work.join("f.txt"), "2").unwrap();
    checkpoint(&m, &work);
    fs::write(work.join("f.txt"), "dirty").unwrap();
    let args = [
        "trace",
        "restore",
        "@~1",
        "--in-place",
        "--idempotency-key",
        "r1",
    ];
    assert_eq!(m.hf(&work, &args).err_code(), "E_WORKTREE_DIRTY");
    checkpoint(&m, &work);
    let r = m.hf(&work, &args);
    assert_eq!(r.err_code(), "E_WORKTREE_DIRTY", "{}", r.stdout);
    assert_eq!(r.json["data"]["replayed"], true);
    assert_eq!(fs::read_to_string(work.join("f.txt")).unwrap(), "dirty");
}

/// A key whose first run has not finished refuses a second run; once the
/// reservation is stale (the first run crashed), the key can be used.
#[test]
fn idempotency_refuses_a_run_in_progress() {
    let root = tempdir("idem-busy");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    m.hf(&work, &["init"]).ok();
    fs::write(work.join("f.txt"), "1").unwrap();
    let args = ["checkpoint", "--idempotency-key", "busy"];
    // Reserve the key exactly as a run in progress would.
    m.hf(&work, &["checkpoint", "--idempotency-key", "probe"])
        .ok();
    let db = store_db(&work);
    let fp: String = db
        .query_row(
            "SELECT fingerprint FROM idempotency WHERE key = 'probe'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fp = fp.replace("probe", "busy");
    db.execute(
        "INSERT INTO idempotency (key, fingerprint, response, at, state) VALUES ('busy', ?1, '', ?2, 0)",
        rusqlite::params![fp, ghf_proto::id::now_ms()],
    )
    .unwrap();
    let r = m.hf(&work, &args);
    assert_eq!(r.err_code(), "E_IDEMPOTENCY_IN_PROGRESS", "{}", r.stdout);
    assert_eq!(r.code, 4);
    let r = m.hf_env(&work, &args, &[("GHF_IDEMPOTENCY_STALE", "0s")]);
    assert_eq!(r.ok()["created"], false);
    assert_eq!(m.hf(&work, &args).ok()["replayed"], true);
}
