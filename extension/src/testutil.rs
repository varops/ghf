//! Unit-test helpers: a throwaway repository opened as a [`Workspace`]
//! without touching the user's configuration directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::RepoConfig;
use crate::git::Repo;
use crate::store::Store;
use crate::workspace::{Workspace, WorktreeContext};

pub fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

/// A repository with one commit, opened as a workspace with its own
/// context. Remove the directory when done.
pub fn workspace(tag: &str) -> (PathBuf, Workspace) {
    let dir = std::env::temp_dir().join(format!("ghf-unit-{tag}-{}", ghf_proto::id::random_hex(6)));
    fs::create_dir_all(&dir).unwrap();
    let dir = crate::platform::canonicalize(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    let ws = open(&dir);
    (dir, ws)
}

/// Open `dir` as a workspace with a fresh context.
pub fn open(dir: &Path) -> Workspace {
    let repo = Repo::discover(dir).unwrap();
    let store = Store::open(&repo.common_dir).unwrap();
    let ctx = WorktreeContext {
        id: ghf_proto::id::new_id(ghf_proto::id::CONTEXT),
        path: dir.to_path_buf(),
        machine_id: "mc_test".into(),
        created_at: ghf_proto::id::now_ms(),
    };
    Workspace {
        repo,
        cfg: RepoConfig::default(),
        store,
        ctx,
        replaced_context: None,
    }
}
