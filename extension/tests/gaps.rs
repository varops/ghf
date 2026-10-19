//! The standalone `ghf` command, the file watcher, staged changes and
//! submodules.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::Value;

fn init(m: &Machine, dir: &Path) {
    m.hf(dir, &["init"]).ok();
}

fn log(m: &Machine, dir: &Path) -> Vec<Value> {
    m.hf(dir, &["trace", "log"]).ok()["checkpoints"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn ghf_is_the_same_command() {
    let root = tempdir("alias");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work);
    let out = Command::new(env!("CARGO_BIN_EXE_ghf"))
        .current_dir(&work)
        .env("GHF_CONFIG_DIR", &m.config)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    let help = Command::new(env!("CARGO_BIN_EXE_ghf"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: ghf"));
}

#[test]
fn watcher_captures_out_of_band_writes_for_the_current_writer() {
    let root = tempdir("watch");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    init(&m, &work);
    // An agent owns the trace.
    fs::write(work.join("agent.txt"), "1").unwrap();
    m.hf_env(&work, &["checkpoint"], &[("GHF_WRITER", "agent-1")])
        .ok();

    let mut child = m
        .cmd(&work)
        .args(["watch", "--debounce", "200ms", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // A generator writes outside any tool call.
    fs::create_dir_all(work.join("gen")).unwrap();
    fs::write(work.join("gen/out.rs"), "// generated").unwrap();

    let start = Instant::now();
    let watched = loop {
        let cks = log(&m, &work);
        if let Some(c) = cks
            .iter()
            .find(|c| c["annotations"]["captured_by"] == "watch")
        {
            break c.clone();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "watcher made no checkpoint"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        watched["author"], "agent-1",
        "recorded for the current writer"
    );
    assert!(
        watched["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == "gen/out.rs")
    );
    // The agent keeps its trace: no lock conflict, and the watcher claimed nothing.
    fs::write(work.join("agent.txt"), "2").unwrap();
    m.hf_env(&work, &["checkpoint"], &[("GHF_WRITER", "agent-1")])
        .ok();
}

#[test]
fn staged_changes_are_captured_and_restaged_on_request() {
    let root = tempdir("index");
    let (_, work) = repo(&root);
    let m = Machine::new(&root, "a");
    fs::write(work.join("a.txt"), "v1\n").unwrap();
    fs::write(work.join("c.txt"), "gone soon\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "files"]);
    init(&m, &work);

    // Partial staging: a.txt staged at v2, edited further to v3 in the tree.
    fs::write(work.join("a.txt"), "v2\n").unwrap();
    git(&work, &["add", "a.txt"]);
    fs::write(work.join("a.txt"), "v3\n").unwrap();
    fs::write(work.join("b.txt"), "new\n").unwrap();
    git(&work, &["add", "b.txt"]);
    git(&work, &["rm", "-q", "c.txt"]);
    let ck = m.hf(&work, &["checkpoint"]).ok()["checkpoint"].clone();
    assert_eq!(ck["index"]["removed"][0], "c.txt");

    // Staging alone is a change worth a checkpoint.
    git(&work, &["add", "a.txt"]);
    let again = m.hf(&work, &["checkpoint"]).ok().clone();
    assert_eq!(again["created"], true);

    // Restore into a new worktree, restaging.
    let id = ck["id"].as_str().unwrap();
    let d = m
        .hf(&work, &["trace", "restore", id, "--index"])
        .ok()
        .clone();
    let wt = Path::new(d["worktree"].as_str().unwrap()).to_path_buf();
    assert_eq!(d["restaged"], 3);
    let staged = git(&wt, &["diff", "--cached", "--name-status"]);
    assert!(
        staged.contains("M\ta.txt") && staged.contains("A\tb.txt") && staged.contains("D\tc.txt"),
        "{staged}"
    );
    assert_eq!(git(&wt, &["show", ":a.txt"]), "v2");
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "v3\n");

    // Without --index the index is left alone, with a warning.
    let d = m.hf(&work, &["trace", "fork", id]).ok().clone();
    let wt2 = Path::new(d["worktree"].as_str().unwrap()).to_path_buf();
    assert!(d["warnings"].to_string().contains("--index"));
    assert_eq!(git(&wt2, &["diff", "--cached", "--name-status"]), "");

    // In place: an uncaptured index change is refused, like an uncaptured edit.
    git(&work, &["reset", "-q"]);
    let r = m.hf(&work, &["trace", "restore", id, "--in-place", "--index"]);
    assert_eq!(r.err_code(), "E_WORKTREE_DIRTY");
    m.hf(&work, &["checkpoint"]).ok();
    m.hf(&work, &["trace", "restore", id, "--in-place", "--index"])
        .ok();
    assert_eq!(git(&work, &["show", ":a.txt"]), "v2");
}

#[test]
fn submodules_are_recorded_and_restored_when_safe() {
    let root = tempdir("submodule");
    let lib = root.join("lib");
    fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q", "-b", "main"]);
    fs::write(lib.join("v"), "1").unwrap();
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-q", "-m", "one"]);
    let c1 = git(&lib, &["rev-parse", "HEAD"]);
    fs::write(lib.join("v"), "2").unwrap();
    git(&lib, &["commit", "-qam", "two"]);
    let c2 = git(&lib, &["rev-parse", "HEAD"]);

    let (_, work) = repo(&root);
    git(
        &work,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib.to_str().unwrap(),
            "lib",
        ],
    );
    git(&work, &["commit", "-q", "-m", "submodule"]);
    let m = Machine::new(&root, "a");
    init(&m, &work);
    let at_c2 = m.hf(&work, &["checkpoint", "--force"]).ok()["checkpoint"].clone();

    git(&work.join("lib"), &["checkout", "-q", "--detach", &c1]);
    let at_c1 = m.hf(&work, &["checkpoint"]).ok()["checkpoint"].clone();
    assert!(
        at_c1["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == "lib")
    );

    // In place: the clean submodule moves back to the recorded commit.
    let d = m
        .hf(
            &work,
            &[
                "trace",
                "restore",
                at_c2["id"].as_str().unwrap(),
                "--in-place",
            ],
        )
        .ok()
        .clone();
    assert!(d["warnings"].as_array().unwrap().is_empty(), "{d}");
    assert_eq!(git(&work.join("lib"), &["rev-parse", "HEAD"]), c2);

    // A new worktree has no initialized submodule: restore still succeeds and says what to run.
    let d = m
        .hf(&work, &["trace", "restore", at_c1["id"].as_str().unwrap()])
        .ok()
        .clone();
    assert!(
        d["warnings"]
            .to_string()
            .contains("git submodule update --init lib"),
        "{d}"
    );
}
