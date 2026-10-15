//! Thin wrapper over the `git` executable. ghf only reads from Git, except
//! `git worktree add` when restoring into a new worktree.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::Result;
use crate::{err, fail};

#[derive(Debug, Clone)]
pub struct Repo {
    /// Worktree root.
    pub root: PathBuf,
    /// This worktree's Git dir (`.git` or `.git/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// Shared Git dir for all worktrees of the repository.
    pub common_dir: PathBuf,
}

fn git_in(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir).stdin(Stdio::null());
    // Never let Git open a pager or prompt.
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_PAGER", "cat");
    c
}

fn run(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd
        .output()
        .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
    if !out.status.success() {
        fail!(
            Internal,
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim_end().to_owned()
}

impl Repo {
    pub fn discover(cwd: &Path) -> Result<Self> {
        let mut c = git_in(cwd);
        c.args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ]);
        let out = c
            .output()
            .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
        if !out.status.success() {
            fail!(NotARepo, "not inside a Git worktree: {}", cwd.display());
        }
        let s = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut lines = s.lines();
        let (Some(root), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next())
        else {
            fail!(NotARepo, "not inside a Git worktree (bare repository?)");
        };
        Ok(Self {
            root: PathBuf::from(root),
            git_dir: PathBuf::from(git_dir),
            common_dir: PathBuf::from(common),
        })
    }

    pub fn git(&self) -> Command {
        git_in(&self.root)
    }

    pub fn run<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut c = self.git();
        c.args(args);
        run(c)
    }

    /// Current HEAD commit, or `None` on an unborn branch.
    pub fn head(&self) -> Option<String> {
        self.resolve_commit("HEAD")
    }

    pub fn resolve_commit(&self, rev: &str) -> Option<String> {
        let mut c = self.git();
        c.args(["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")]);
        let out = c.output().ok()?;
        out.status.success().then(|| text(out.stdout))
    }

    pub fn is_ancestor(&self, commit: &str, of: &str) -> bool {
        let mut c = self.git();
        c.args(["merge-base", "--is-ancestor", commit, of]);
        c.status().is_ok_and(|s| s.success())
    }

    /// Captured paths: tracked plus untracked, minus anything Git ignores.
    pub fn list_files(&self) -> Result<Vec<Vec<u8>>> {
        let out = self.run([
            "-c",
            "core.quotepath=off",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])?;
        let mut paths: Vec<Vec<u8>> = out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    pub fn remote_url(&self, name: &str) -> Option<String> {
        let mut c = self.git();
        c.args(["remote", "get-url", name]);
        let out = c.output().ok()?;
        out.status.success().then(|| text(out.stdout))
    }

    /// `git worktree add --detach <path> <rev>`.
    pub fn worktree_add(&self, path: &Path, rev: &str) -> Result<()> {
        let mut c = self.git();
        c.args(["worktree", "add", "--detach", "--quiet"])
            .arg(path)
            .arg(rev);
        run(c).map(|_| ())
    }

    /// Three-way merge of file contents with `git merge-file`.
    /// Returns the merged bytes and whether it has conflicts.
    pub fn merge_file(&self, ours: &Path, base: &Path, theirs: &Path) -> Result<(Vec<u8>, bool)> {
        let mut c = self.git();
        c.args([
            "merge-file",
            "-p",
            "-L",
            "working tree",
            "-L",
            "base",
            "-L",
            "incoming",
        ])
        .arg(ours)
        .arg(base)
        .arg(theirs);
        let out = c
            .output()
            .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
        match out.status.code() {
            Some(0) => Ok((out.stdout, false)),
            Some(n) if n > 0 && n < 128 => Ok((out.stdout, true)),
            _ => fail!(
                Internal,
                "git merge-file failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// Paths changed relative to HEAD (`git status`), used for the first
    /// checkpoint of a trace. Returns (status letter, path) pairs.
    pub fn status_vs_head(&self) -> Result<Vec<(char, String)>> {
        let out = self.run([
            "-c",
            "core.quotepath=off",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])?;
        let mut v = Vec::new();
        for rec in out.split(|b| *b == 0).filter(|r| r.len() > 3) {
            let x = rec[0] as char;
            let y = rec[1] as char;
            let path = String::from_utf8_lossy(&rec[3..]).into_owned();
            let code = match (x, y) {
                ('?', '?') | ('A', _) => 'A',
                (_, 'D') | ('D', _) => 'D',
                _ => 'M',
            };
            v.push((code, path));
        }
        Ok(v)
    }
}

/// Derive `owner/name` from a remote URL such as
/// `git@github.com:org/project.git` or `https://host/org/project`.
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let tail = trimmed.rsplit_once(['/', ':']).map(|(head, name)| {
        let owner = head.rsplit(['/', ':']).next().unwrap_or_default();
        format!("{owner}/{name}")
    })?;
    ghf_proto::api::valid_repo(&tail).then_some(tail)
}

#[cfg(test)]
mod tests {
    use super::repo_name_from_url;

    #[test]
    fn repo_names_from_urls() {
        assert_eq!(
            repo_name_from_url("git@github.com:varops/ghf.git").as_deref(),
            Some("varops/ghf")
        );
        assert_eq!(
            repo_name_from_url("https://github.com/varops/ghf").as_deref(),
            Some("varops/ghf")
        );
        assert_eq!(repo_name_from_url("ghf").as_deref(), None);
    }
}
