//! End-to-end acceptance scenarios, run against real Git repositories and a
//! real `ghf-server`.

mod common;

use std::fs;

use common::*;
use serde_json::json;

fn init(m: &Machine, dir: &std::path::Path, server: Option<&Server>) {
    let mut args = vec!["init"];
    if let Some(s) = server {
        args.extend(["--server", &s.url, "--repository", "acme/proj", "--trust"]);
    }
    m.hf(dir, &args).ok();
}

fn checkpoint(m: &Machine, dir: &std::path::Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["checkpoint"];
    args.extend(extra);
    m.hf(dir, &args).ok()["checkpoint"].clone()
}

// 1. Plain repo, `git hf init`, trust the server.
#[test]
fn s01_init_and_trust() {
    let root = tempdir("s01");
    let server = Server::start(&root, &[]);
    let (_, work) = repo(&root);
    let m = connected(&root, "a", &server);
    let r = m.hf(
        &work,
        &[
            "init",
            "--server",
            &server.url,
            "--repository",
            "acme/proj",
            "--trust",
        ],
    );
    let d = r.ok();
    assert_eq!(d["created"], true);
    assert_eq!(d["trusted"], true);
    let text = fs::read_to_string(work.join(".ghf")).unwrap();
    assert!(text.contains("acme/proj") && !text.contains(&server.token));
    // Re-running is a no-op; changing settings needs --force.
    assert_eq!(m.hf(&work, &["init"]).ok()["created"], false);
    let r = m.hf(&work, &["init", "--server", &server.url]);
    assert_eq!(r.err_code(), "E_PATH_EXISTS");

    // An untrusted machine refuses to upload and never prompts.
    let other = connected(&root, "b", &server);
    let r = other.hf(&work, &["sync"]);
    assert_eq!(r.err_code(), "E_UNTRUSTED_SERVER");
}

// 2. Two worktrees get separate contexts and traces; a second writer is refused.
#[test]
fn s02_worktrees_and_writer_lock() {
    let root = tempdir("s02");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    let wt2 = root.join("wt2");
    git(
        &work,
        &["worktree", "add", "-q", "--detach", wt2.to_str().unwrap()],
    );
    fs::copy(work.join(".ghf"), wt2.join(".ghf")).unwrap();

    fs::write(work.join("a.txt"), "1").unwrap();
    fs::write(wt2.join("b.txt"), "1").unwrap();
    let a = m
        .hf_env(&work, &["checkpoint"], &[("GHF_WRITER", "agent-a")])
        .ok()
        .clone();
    let b = m
        .hf_env(&wt2, &["checkpoint"], &[("GHF_WRITER", "agent-b")])
        .ok()
        .clone();
    assert_ne!(a["checkpoint"]["context_id"], b["checkpoint"]["context_id"]);
    assert_ne!(a["trace"]["id"], b["trace"]["id"]);

    fs::write(work.join("a.txt"), "2").unwrap();
    let r = m.hf_env(&work, &["checkpoint"], &[("GHF_WRITER", "agent-b")]);
    assert_eq!(r.err_code(), "E_WRITER_LOCKED");
    assert_eq!(r.code, 3);
    assert_eq!(r.json["data"]["writer"], "agent-a");
    // The owner keeps writing; an explicit takeover is allowed.
    m.hf_env(&work, &["checkpoint"], &[("GHF_WRITER", "agent-a")])
        .ok();
    m.hf_env(
        &work,
        &["checkpoint", "--takeover", "--force"],
        &[("GHF_WRITER", "agent-b")],
    )
    .ok();
}

// 3. Hooks create checkpoints with no agent action.
#[test]
fn s03_hooks_capture_automatically() {
    let root = tempdir("s03");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    m.hf(&work, &["hooks", "install", "claude-code"]).ok();
    m.hf(&work, &["hooks", "install", "cursor"]).ok();
    let settings = fs::read_to_string(work.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("git hf hook claude-code"));

    let claude = |file: &str| {
        json!({
            "session_id": "s1", "cwd": work, "hook_event_name": "PostToolUse",
            "tool_name": "Write", "tool_input": {"file_path": work.join(file)},
        })
    };
    for i in 0..5 {
        fs::write(work.join("src.txt"), format!("v{i}")).unwrap();
        let (code, out) = m.hook(&work, "claude-code", &claude("src.txt"));
        assert_eq!((code, out.as_str()), (0, ""));
    }
    // An edit that changed nothing does not add a checkpoint.
    m.hook(&work, "claude-code", &claude("src.txt"));
    let log = m.hf(&work, &["trace", "log"]);
    let cks = log.ok()["checkpoints"].as_array().unwrap().clone();
    assert_eq!(cks.len(), 5);
    assert_eq!(cks[4]["author"], "claude-code:s1");

    // A different session in the same worktree is a second writer: refused,
    // reported to the agent, and the run continues (best-effort).
    fs::write(work.join("src.txt"), "intruder").unwrap();
    let (code, out) = m.hook(
        &work,
        "claude-code",
        &json!({"session_id": "other", "cwd": work}),
    );
    assert_eq!(code, 0);
    assert!(out.contains("E_WRITER_LOCKED"), "{out}");

    // Cursor payloads locate the repository from the edited file.
    let wt = root.join("cursor-wt");
    git(
        &work,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    fs::copy(work.join(".ghf"), wt.join(".ghf")).unwrap();
    fs::write(wt.join("c.txt"), "cursor").unwrap();
    let (code, _) = m.hook(
        &root,
        "cursor",
        &json!({"conversation_id": "c1", "hook_event_name": "afterFileEdit",
                "file_path": wt.join("c.txt"), "workspace_roots": [wt]}),
    );
    assert_eq!(code, 0);
    let log = m.hf(&wt, &["trace", "log"]).ok().clone();
    assert_eq!(log["checkpoints"][0]["author"], "cursor:c1");

    // Outside a ghf repository the hook does nothing.
    let plain = root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    git(&plain, &["init", "-q"]);
    let (code, out) = m.hook(
        &plain,
        "claude-code",
        &json!({"session_id": "s", "cwd": plain}),
    );
    assert_eq!((code, out.as_str()), (0, ""));
}

// 4. Strict mode fails the tool call with a stable code; best-effort warns and continues.
#[test]
fn s04_capture_failure_modes() {
    let root = tempdir("s04");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    checkpoint(&m, &work, &[]);
    // Break the local store.
    let db = work.join(".git/ghf/ghf.db");
    fs::write(&db, b"this is not a database").unwrap();
    let _ = fs::remove_file(work.join(".git/ghf/ghf.db-wal"));
    let _ = fs::remove_file(work.join(".git/ghf/ghf.db-shm"));
    fs::write(work.join("x.txt"), "x").unwrap();
    let payload = json!({"session_id": "s1", "cwd": work});

    let (code, out) = m
        .clone()
        .with("GHF_STRICT", "1")
        .hook(&work, "claude-code", &payload);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["continue"], false);
    assert!(
        v["stopReason"]
            .as_str()
            .unwrap()
            .contains("E_CAPTURE_FAILED")
    );

    let (code, out) = m
        .clone()
        .with("GHF_STRICT", "0")
        .hook(&work, "claude-code", &payload);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.get("continue").is_none());
    assert!(v["systemMessage"].as_str().unwrap().contains("best-effort"));
    // The failure is recorded where `status` can still find it.
    assert!(
        fs::read_to_string(work.join(".git/ghf/failures.log"))
            .unwrap()
            .contains("strict")
    );

    // Cursor in strict mode exits non-zero.
    let (code, _) = m.clone().with("GHF_STRICT", "1").hook(
        &work,
        "cursor",
        &json!({"workspace_roots": [work]}),
    );
    assert_ne!(code, 0);
}

// 5. Acknowledged checkpoints survive a killed worker.
#[test]
fn s05_crash_durability() {
    let root = tempdir("s05");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    let mut acked = Vec::new();
    for i in 0..10 {
        fs::write(work.join("f.txt"), format!("{i}")).unwrap();
        acked.push(
            checkpoint(&m, &work, &[])["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    // Kill a capture mid-flight; it must not damage what was acknowledged.
    fs::write(work.join("big.bin"), vec![7u8; 8 * 1024 * 1024]).unwrap();
    let mut child = m.cmd(&work).args(["checkpoint", "--json"]).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(15));
    let _ = child.kill();
    let _ = child.wait();
    let log = m.hf(&work, &["trace", "log"]);
    let ids: Vec<String> = log.ok()["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    for id in &acked {
        assert!(ids.contains(id), "lost acknowledged checkpoint {id}");
    }
    // And capture still works afterwards.
    fs::write(work.join("f.txt"), "after").unwrap();
    checkpoint(&m, &work, &[]);
}

// 6. Offline work syncs later with no duplicates.
#[test]
fn s06_offline_then_sync() {
    let root = tempdir("s06");
    let mut server = Server::start(&root, &[]);
    let (_, work) = repo(&root);
    let m = connected(&root, "a", &server);
    init(&m, &work, Some(&server));
    server.stop();
    for i in 0..4 {
        fs::write(work.join("f.txt"), format!("{i}")).unwrap();
        checkpoint(&m, &work, &[]);
    }
    let r = m.hf(&work, &["sync"]);
    assert_eq!(r.err_code(), "E_SERVER");
    assert_eq!(r.code, 5);
    assert_eq!(r.json["data"]["pending"], 4);
    assert_eq!(m.hf(&work, &["status"]).ok()["unsynced"], 4);

    server.restart();
    let d = m.hf(&work, &["sync"]).ok().clone();
    assert_eq!(d["pushed_checkpoints"], 4);
    assert_eq!(d["pending"], 0);
    let d = m.hf(&work, &["sync"]).ok().clone();
    assert_eq!(d["pushed_checkpoints"], 0);
    assert_eq!(d["pushed_objects"], 0);
    // A fresh machine sees exactly four checkpoints.
    let b = connected(&root, "b", &server);
    let clone_b = clone(&root.join("origin.git"), &root.join("b"));
    fs::copy(work.join(".ghf"), clone_b.join(".ghf")).unwrap();
    b.hf(&clone_b, &["trust"]).ok();
    b.hf(&clone_b, &["sync"]).ok();
    let traces = b.hf(&clone_b, &["trace", "list"]).ok()["traces"].clone();
    assert_eq!(traces.as_array().unwrap().len(), 1);
    assert_eq!(traces[0]["checkpoints"], 4);
}

// 7. Restore with no flags lands in a new worktree; the original is untouched.
#[test]
fn s07_restore_into_new_worktree() {
    let root = tempdir("s07");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    fs::create_dir_all(work.join("src/deep")).unwrap();
    fs::write(work.join("src/deep/a.rs"), "one").unwrap();
    fs::write(work.join("run.sh"), "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(work.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/deep/a.rs", work.join("link")).unwrap();
    let first = checkpoint(&m, &work, &[]);
    fs::write(work.join("src/deep/a.rs"), "two").unwrap();
    fs::remove_file(work.join("run.sh")).unwrap();
    checkpoint(&m, &work, &[]);
    fs::write(work.join("uncaptured.txt"), "mine").unwrap();
    let before = fs::read_to_string(work.join("src/deep/a.rs")).unwrap();

    let d = m
        .hf(&work, &["trace", "restore", first["id"].as_str().unwrap()])
        .ok()
        .clone();
    let dest = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(dest.join("src/deep/a.rs")).unwrap(),
        "one"
    );
    assert_eq!(
        fs::metadata(dest.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        fs::read_link(dest.join("link")).unwrap().to_str().unwrap(),
        "src/deep/a.rs"
    );
    assert_ne!(d["context"], m.hf(&work, &["status"]).ok()["context"]);
    assert_eq!(
        d["checkpoint"]["root"], first["root"],
        "byte-identical restore"
    );
    assert_eq!(
        fs::read_to_string(work.join("src/deep/a.rs")).unwrap(),
        before
    );
    assert!(work.join("uncaptured.txt").exists());
    // The new worktree has its own trace, forked from the checkpoint.
    let log = m.hf(&dest, &["trace", "log"]).ok().clone();
    assert_eq!(log["trace"]["forked_from"], first["id"]);
    // Restoring onto an existing directory is refused.
    let r = m.hf(
        &work,
        &[
            "trace",
            "restore",
            first["id"].as_str().unwrap(),
            "--worktree",
            dest.to_str().unwrap(),
        ],
    );
    assert_eq!(r.err_code(), "E_PATH_EXISTS");
}

// 8. In-place restore refuses to lose uncaptured work.
#[test]
fn s08_in_place_restore_guard() {
    let root = tempdir("s08");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    fs::write(work.join("f.txt"), "green").unwrap();
    let green = checkpoint(&m, &work, &["--annotate", "tests=pass"]);
    fs::write(work.join("f.txt"), "red").unwrap();
    fs::write(work.join("new.txt"), "new").unwrap();
    checkpoint(&m, &work, &["--annotate", "tests=fail"]);
    fs::write(work.join("f.txt"), "uncaptured").unwrap();

    let id = green["id"].as_str().unwrap();
    let r = m.hf(&work, &["trace", "restore", id, "--in-place"]);
    assert_eq!(r.err_code(), "E_WORKTREE_DIRTY");
    assert_eq!(
        fs::read_to_string(work.join("f.txt")).unwrap(),
        "uncaptured"
    );

    checkpoint(&m, &work, &[]);
    let d = m
        .hf(&work, &["trace", "restore", id, "--in-place"])
        .ok()
        .clone();
    assert_eq!(fs::read_to_string(work.join("f.txt")).unwrap(), "green");
    assert!(!work.join("new.txt").exists());
    assert_eq!(d["checkpoint"]["annotations"]["restored_from"], id);
    // Last green is findable by annotation.
    let log = m
        .hf(&work, &["trace", "log", "--annotation", "tests=pass"])
        .ok()
        .clone();
    assert_eq!(log["checkpoints"].as_array().unwrap().len(), 1);

    // --force checkpoints first, so even a forced restore can be undone.
    fs::write(work.join("f.txt"), "precious").unwrap();
    let d = m
        .hf(&work, &["trace", "restore", id, "--in-place", "--force"])
        .ok()
        .clone();
    let pre = d["pre_restore_checkpoint"].as_str().unwrap().to_owned();
    m.hf(&work, &["trace", "restore", &pre, "--in-place"]).ok();
    assert_eq!(fs::read_to_string(work.join("f.txt")).unwrap(), "precious");
}

// 9. Apply stops on conflicts, reports them in JSON, leaves the tree unchanged.
#[test]
fn s09_apply_conflicts() {
    let root = tempdir("s09");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    fs::write(work.join("f.txt"), "a\nb\nc\n").unwrap();
    fs::write(work.join("g.txt"), "1\n").unwrap();
    let base = checkpoint(&m, &work, &[]);
    fs::write(work.join("f.txt"), "a\nB\nc\n").unwrap();
    fs::write(work.join("g.txt"), "1\n2\n").unwrap();
    let tip = checkpoint(&m, &work, &[]);
    let range = format!(
        "{}..{}",
        base["id"].as_str().unwrap(),
        tip["id"].as_str().unwrap()
    );

    // Fork from base, make a conflicting edit, try to apply.
    let fork = m
        .hf(&work, &["trace", "fork", base["id"].as_str().unwrap()])
        .ok()
        .clone();
    let wt = std::path::PathBuf::from(fork["worktree"].as_str().unwrap());
    fs::write(wt.join("f.txt"), "a\nX\nc\n").unwrap();
    let r = m.hf(&wt, &["trace", "apply", &range]);
    assert_eq!(r.err_code(), "E_APPLY_CONFLICT");
    assert_eq!(r.code, 4);
    assert_eq!(r.json["data"]["conflicts"][0]["path"], "f.txt");
    assert_eq!(fs::read_to_string(wt.join("f.txt")).unwrap(), "a\nX\nc\n");
    assert_eq!(fs::read_to_string(wt.join("g.txt")).unwrap(), "1\n");

    // A non-overlapping edit merges cleanly and is recorded.
    fs::write(wt.join("f.txt"), "a\nB\nc\nd\n").unwrap();
    let d = m.hf(&wt, &["trace", "apply", &range]).ok().clone();
    assert_eq!(fs::read_to_string(wt.join("g.txt")).unwrap(), "1\n2\n");
    assert_eq!(
        fs::read_to_string(wt.join("f.txt")).unwrap(),
        "a\nB\nc\nd\n"
    );
    assert!(d["pre_apply_checkpoint"].is_string());
    assert_eq!(d["checkpoint"]["annotations"]["applied_to"], tip["id"]);

    // --write-conflicts leaves markers.
    fs::write(wt.join("g.txt"), "1\nmine\n").unwrap();
    let r = m.hf(&wt, &["trace", "apply", &range, "--write-conflicts"]);
    assert_eq!(r.err_code(), "E_APPLY_CONFLICT");
    assert_eq!(r.json["data"]["applied"], true);
    assert!(
        fs::read_to_string(wt.join("g.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
}

// 10. Fork, then continue on a second machine in a new context.
// 11. Prune the parent; the fork's base stays restorable.
#[test]
fn s10_s11_fork_handoff_and_prune() {
    let root = tempdir("s10");
    let server = Server::start(&root, &["--after-finalize", "0s", "--prune-interval", "0s"]);
    let (origin, work) = repo(&root);
    let a = connected(&root, "a", &server);
    init(&a, &work, Some(&server));
    git(&work, &["add", ".ghf"]);
    git(&work, &["commit", "-q", "-m", "ghf"]);
    git(&work, &["push", "-q", "origin", "main"]);
    fs::write(work.join("f.txt"), "v1").unwrap();
    let base = checkpoint(&a, &work, &[]);
    fs::write(work.join("f.txt"), "v2").unwrap();
    let other = checkpoint(&a, &work, &[]);
    a.hf(&work, &["sync"]).ok();

    // Machine B forks from the base checkpoint and keeps working.
    let b = connected(&root, "b", &server);
    let wb = clone(&origin, &root.join("b"));
    b.hf(&wb, &["trust"]).ok();
    let base_id = base["id"].as_str().unwrap();
    let fork = b.hf(&wb, &["trace", "fork", base_id]).ok().clone();
    let fwt = std::path::PathBuf::from(fork["worktree"].as_str().unwrap());
    assert_eq!(fs::read_to_string(fwt.join("f.txt")).unwrap(), "v1");
    fs::write(fwt.join("f.txt"), "v1-b").unwrap();
    checkpoint(&b, &fwt, &[]);
    b.hf(&fwt, &["sync"]).ok();

    // A abandons the original trace; it becomes eligible and is pruned.
    a.hf(&work, &["trace", "finalize", "--abandoned"]).ok();
    a.hf(&work, &["sync"]).ok();
    let d = a.hf(&work, &["prune"]).ok().clone();
    assert_eq!(d["server"]["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(d["server"]["retained_checkpoints"][0], base["id"]);

    // A third machine can still restore the fork base, but not the pruned one.
    let c = connected(&root, "c", &server);
    let wc = clone(&origin, &root.join("c"));
    c.hf(&wc, &["trust"]).ok();
    let r = c.hf(&wc, &["trace", "restore", base_id]);
    assert_eq!(
        fs::read_to_string(
            std::path::PathBuf::from(r.ok()["worktree"].as_str().unwrap()).join("f.txt")
        )
        .unwrap(),
        "v1"
    );
    let r = c.hf(&wc, &["trace", "restore", other["id"].as_str().unwrap()]);
    assert_eq!(r.err_code(), "E_NOT_FOUND");
}

// 12. Normal Git commit, push, merge; finalize against the merged commit.
// 13. Wait for eligibility; prune.
// 15. Git history, refs and objects are exactly what Git alone would produce.
#[test]
fn s12_s13_s15_finalize_prune_and_git_untouched() {
    let root = tempdir("s12");
    let server = Server::start(&root, &["--after-finalize", "0s", "--prune-interval", "0s"]);
    let (origin, work) = repo(&root);
    let m = connected(&root, "a", &server);
    init(&m, &work, Some(&server));
    git(&work, &["add", ".ghf"]);
    git(&work, &["commit", "-q", "-m", "enable ghf"]);
    let before = git_state(&work);

    fs::write(work.join("feature.txt"), "work in progress").unwrap();
    checkpoint(&m, &work, &[]);
    fs::write(work.join("feature.txt"), "done").unwrap();
    checkpoint(&m, &work, &["-m", "done", "--annotate", "tests=pass"]);
    m.hf(&work, &["trace", "log"]).ok();
    m.hf(&work, &["trace", "diff", "@~1", "@"]).ok();
    m.hf(&work, &["sync"]).ok();
    m.hf(&work, &["status"]).ok();
    // ghf itself wrote nothing into Git.
    assert_eq!(git_state(&work), before);

    git(&work, &["add", "feature.txt"]);
    git(&work, &["commit", "-q", "-m", "add feature"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    let log = git(&work, &["log", "--format=%B"]);
    assert!(!log.contains("ghf") || log.contains("enable ghf"));
    assert!(!log.contains("ck_") && !log.contains("tr_"));

    // Unreachable commits are refused.
    git(&work, &["checkout", "-q", "-b", "side"]);
    fs::write(work.join("side.txt"), "x").unwrap();
    git(&work, &["add", "side.txt"]);
    git(&work, &["commit", "-q", "-m", "side"]);
    let side = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "main"]);
    assert_eq!(
        m.hf(&work, &["trace", "finalize", "--commit", &side])
            .err_code(),
        "E_COMMIT_NOT_REACHABLE"
    );
    assert_eq!(
        m.hf(&work, &["trace", "finalize", "--commit", "deadbeef"])
            .err_code(),
        "E_COMMIT_NOT_FOUND"
    );

    git(&work, &["fetch", "-q", "origin"]);
    let d = m
        .hf(&work, &["trace", "finalize", "--commit", &sha])
        .ok()
        .clone();
    assert_eq!(d["trace"]["final_commit"], sha);
    // Pinned traces survive pruning until released.
    let trace = d["trace"]["id"].as_str().unwrap().to_owned();
    m.hf(&work, &["trace", "pin", &trace]).ok();
    let d = m.hf(&work, &["prune"]).ok().clone();
    assert!(d["server"]["pruned"].as_array().unwrap().is_empty());
    m.hf(&work, &["trace", "unpin", &trace]).ok();
    let d = m.hf(&work, &["prune"]).ok().clone();
    assert_eq!(d["server"]["pruned"][0], trace.as_str());
    assert!(d["server"]["objects_deleted"].as_u64().unwrap() > 0);
    assert_eq!(d["local"]["pruned"][0], trace.as_str());
    let list = m.hf(&work, &["trace", "list", "--all"]).ok().clone();
    assert_eq!(list["traces"][0]["state"], "pruned");

    // A machine without ghf sees an ordinary repository.
    let plain = clone(&origin, &root.join("plain"));
    assert_eq!(
        git(&plain, &["log", "--format=%s"]),
        "add feature\nenable ghf\ninit"
    );
    assert!(
        git(&plain, &["for-each-ref"])
            .lines()
            .all(|l| !l.contains("ghf"))
    );
}

// 14. A closed trace that is never finalized warns, then becomes eligible.
#[test]
fn s14_unfinalized_cap() {
    let root = tempdir("s14");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    let cfg = fs::read_to_string(work.join(".ghf")).unwrap();
    fs::write(
        work.join(".ghf"),
        format!("{cfg}\n[retention]\nunfinalized_cap = \"7d\"\n"),
    )
    .unwrap();
    fs::write(work.join("f.txt"), "x").unwrap();
    checkpoint(&m, &work, &[]);
    m.hf(&work, &["trace", "close"]).ok();
    let st = m.hf(&work, &["status"]).ok().clone();
    assert!(
        st["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("not finalized"))
    );
    fs::write(
        work.join(".ghf"),
        format!("{cfg}\n[retention]\nunfinalized_cap = \"0s\"\n"),
    )
    .unwrap();
    let d = m
        .hf_env(&work, &["prune"], &[("GHF_GC_GRACE", "0s")])
        .ok()
        .clone();
    assert_eq!(d["local"]["pruned"].as_array().unwrap().len(), 1);
    assert!(d["local"]["objects_deleted"].as_u64().unwrap() > 0);
}

// CLI contract: JSON envelope, stable codes, idempotency, no prompts.
#[test]
fn cli_contract() {
    let root = tempdir("cli");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    let r = m.hf(&root, &["status"]);
    assert_eq!(r.err_code(), "E_NOT_A_REPO");
    let r = m.hf(&work, &["status"]);
    assert_eq!(r.err_code(), "E_NOT_INITIALIZED");
    let r = m.hf(&work, &["trace", "finalize"]);
    assert_eq!((r.code, r.err_code()), (2, "E_USAGE"));
    init(&m, &work, None);
    let r = m.hf(&work, &["status"]);
    assert_eq!(r.json["schema"], "ghf/v1");
    assert_eq!(r.json["command"], "status");

    fs::write(work.join("f.txt"), "x").unwrap();
    let first = m
        .hf(&work, &["checkpoint", "--force", "--idempotency-key", "k1"])
        .ok()
        .clone();
    let again = m
        .hf(&work, &["checkpoint", "--force", "--idempotency-key", "k1"])
        .ok()
        .clone();
    assert_eq!(again["replayed"], true);
    assert_eq!(
        again["result"]["checkpoint"]["id"],
        first["checkpoint"]["id"]
    );
    let r = m.hf(
        &work,
        &["checkpoint", "-m", "different", "--idempotency-key", "k1"],
    );
    assert_eq!(r.err_code(), "E_IDEMPOTENCY_CONFLICT");
    assert_eq!(
        m.hf(&work, &["trace", "log"]).ok()["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let r = m.hf(&work, &["trace", "restore", "ck_zzzzzzzz"]);
    assert_eq!((r.code, r.err_code()), (6, "E_NOT_FOUND"));
}

// .ghfignore paths never enter a checkpoint; .gitignore is respected too.
#[test]
fn ignore_rules() {
    let root = tempdir("ign");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    fs::write(work.join(".ghfignore"), "secrets/\n*.key\n").unwrap();
    fs::create_dir_all(work.join("secrets")).unwrap();
    fs::write(work.join("secrets/token"), "s3cret").unwrap();
    fs::write(work.join("id.key"), "k").unwrap();
    fs::create_dir_all(work.join("target")).unwrap();
    fs::write(work.join("target/out"), "o").unwrap();
    fs::write(work.join("kept.txt"), "k").unwrap();
    let ck = checkpoint(&m, &work, &[]);
    let paths: Vec<&str> = ck["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"kept.txt"));
    assert!(
        !paths
            .iter()
            .any(|p| p.contains("secrets") || p.ends_with(".key") || p.starts_with("target"))
    );
    // Restoring never touches ignored files.
    fs::write(work.join("kept.txt"), "changed").unwrap();
    checkpoint(&m, &work, &[]);
    m.hf(
        &work,
        &["trace", "restore", ck["id"].as_str().unwrap(), "--in-place"],
    )
    .ok();
    assert_eq!(
        fs::read_to_string(work.join("secrets/token")).unwrap(),
        "s3cret"
    );
    assert!(work.join("target/out").exists());
}

// Review regressions: restore and apply never clobber files ghf did not
// capture, and file/directory collisions refuse the apply up front.
#[test]
fn never_overwrites_uncaptured_files() {
    let root = tempdir("clobber");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work, None);
    fs::write(work.join("settings.env"), "captured").unwrap();
    let with_env = checkpoint(&m, &work, &[]);
    fs::remove_file(work.join("settings.env")).unwrap();
    checkpoint(&m, &work, &[]);
    // Locally ignored now, and holding precious local content.
    fs::write(work.join(".git/info/exclude"), "settings.env\n").unwrap();
    fs::write(work.join("settings.env"), "local secret").unwrap();
    checkpoint(&m, &work, &["--force"]);
    let r = m.hf(
        &work,
        &[
            "trace",
            "restore",
            with_env["id"].as_str().unwrap(),
            "--in-place",
        ],
    );
    assert_eq!(r.err_code(), "E_WORKTREE_DIRTY");
    assert_eq!(r.json["data"]["paths"][0], "settings.env");
    assert_eq!(
        fs::read_to_string(work.join("settings.env")).unwrap(),
        "local secret"
    );

    // Apply reports it as a conflict and leaves everything alone.
    let base = checkpoint(&m, &work, &["--force"]);
    fs::write(work.join("x"), "file").unwrap();
    let tip = checkpoint(&m, &work, &[]);
    fs::remove_file(work.join("x")).unwrap();
    fs::create_dir_all(work.join("x")).unwrap();
    fs::write(work.join("x/inner.txt"), "dir now").unwrap();
    checkpoint(&m, &work, &[]);
    let range = format!(
        "{}..{}",
        base["id"].as_str().unwrap(),
        tip["id"].as_str().unwrap()
    );
    let r = m.hf(&work, &["trace", "apply", &range, "--write-conflicts"]);
    assert_eq!(r.err_code(), "E_APPLY_CONFLICT");
    assert_eq!(r.json["data"]["applied"], false);
    assert_eq!(
        fs::read_to_string(work.join("x/inner.txt")).unwrap(),
        "dir now"
    );
}
