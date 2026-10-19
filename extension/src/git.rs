//! Thin wrapper over the `git` executable. ghf only reads from Git, except
//! `git worktree add` when restoring into a new worktree.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::Result;
use crate::{err, fail};

/// One staged change from `git diff --cached --raw`.
#[derive(Debug, Clone)]
pub struct StagedEntry {
    pub path: String,
    /// Git mode of the staged entry, e.g. `100644`, `100755`, `120000`, `160000`.
    pub mode: String,
    /// Object id of the staged content (a commit id for submodules).
    pub oid: String,
    pub deleted: bool,
}

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

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn common_dir_of(git_dir: &Path) -> PathBuf {
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(t) => {
            let p = git_dir.join(t.trim());
            crate::platform::canonicalize(&p).unwrap_or(p)
        }
        Err(_) => git_dir.to_path_buf(),
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim_end().to_owned()
}

impl Repo {
    pub fn discover(cwd: &Path) -> Result<Self> {
        // Hooks run on every edit; avoid spawning git when the layout is plain.
        if std::env::var_os("GIT_DIR").is_none()
            && std::env::var_os("GIT_WORK_TREE").is_none()
            && let Some(r) = Self::discover_fast(cwd)
        {
            return Ok(r);
        }
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

    /// Find `.git` by walking up from `cwd`. Handles linked worktrees and
    /// submodules (`.git` files). Returns `None` for anything unusual so the
    /// caller can ask git itself.
    fn discover_fast(cwd: &Path) -> Option<Self> {
        let start = crate::platform::canonicalize(cwd).ok()?;
        let mut dir = start.as_path();
        loop {
            let dotgit = dir.join(".git");
            match fs::symlink_metadata(&dotgit) {
                Ok(md) if md.is_dir() => {
                    if !dotgit.join("HEAD").is_file() {
                        return None;
                    }
                    let common = common_dir_of(&dotgit);
                    return Some(Self {
                        root: dir.to_path_buf(),
                        git_dir: dotgit,
                        common_dir: common,
                    });
                }
                Ok(md) if md.is_file() => {
                    let text = fs::read_to_string(&dotgit).ok()?;
                    let p = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
                    let git_dir = crate::platform::canonicalize(dir.join(p)).ok()?;
                    if !git_dir.join("HEAD").is_file() {
                        return None;
                    }
                    let common = common_dir_of(&git_dir);
                    return Some(Self {
                        root: dir.to_path_buf(),
                        git_dir,
                        common_dir: common,
                    });
                }
                Ok(_) => return None,
                Err(_) => dir = dir.parent()?,
            }
        }
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
        match self.head_fast() {
            Some(h) => h,
            None => self.resolve_commit("HEAD"),
        }
    }

    /// Read HEAD from loose or packed refs. `Some(None)` means unborn;
    /// `None` means "ask git" (reftable, symbolic oddities).
    fn head_fast(&self) -> Option<Option<String>> {
        let head = fs::read_to_string(self.git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        let Some(name) = head.strip_prefix("ref: ") else {
            return is_sha(head).then(|| Some(head.to_owned()));
        };
        if self.common_dir.join("reftable").exists() {
            return None;
        }
        for base in [&self.git_dir, &self.common_dir] {
            if let Ok(s) = fs::read_to_string(base.join(name)) {
                let s = s.trim();
                return if is_sha(s) {
                    Some(Some(s.to_owned()))
                } else {
                    None
                };
            }
        }
        if let Ok(packed) = fs::read_to_string(self.common_dir.join("packed-refs")) {
            for line in packed.lines() {
                if let Some((sha, n)) = line.split_once(' ')
                    && n == name
                    && is_sha(sha)
                {
                    return Some(Some(sha.to_owned()));
                }
            }
        }
        Some(None)
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

    /// Staged changes: the index compared with HEAD (or the empty tree on an
    /// unborn branch).
    pub fn staged(&self) -> Result<Vec<StagedEntry>> {
        let out = self.run([
            "-c",
            "core.quotepath=off",
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
            "--ignore-submodules=none",
        ])?;
        let mut v = Vec::new();
        let mut parts = out.split(|b| *b == 0).filter(|p| !p.is_empty());
        while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
            let meta = String::from_utf8_lossy(meta);
            let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            if f.len() < 5 {
                continue;
            }
            v.push(StagedEntry {
                path: String::from_utf8_lossy(path).into_owned(),
                mode: f[1].to_owned(),
                oid: f[3].to_owned(),
                deleted: f[4].starts_with('D'),
            });
        }
        Ok(v)
    }

    /// Contents of Git blobs, by object id, via one `git cat-file --batch`.
    pub fn cat_blobs(&self, oids: &[String]) -> Result<Vec<Vec<u8>>> {
        use std::io::{BufRead, BufReader, Read, Write};
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let mut child = self
            .git()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut blobs = Vec::with_capacity(oids.len());
        for oid in oids {
            let mut header = String::new();
            out.read_line(&mut header)?;
            let size: usize = header
                .split_whitespace()
                .nth(2)
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| err!(Internal, "git cat-file has no object {oid}"))?;
            let mut buf = vec![0u8; size + 1];
            out.read_exact(&mut buf)?;
            buf.pop();
            blobs.push(buf);
        }
        let _ = writer.join();
        let _ = child.wait();
        Ok(blobs)
    }

    /// Reset the index to `rev`'s tree without touching the working tree.
    pub fn read_tree(&self, rev: &str) -> Result<()> {
        self.run(["read-tree", rev]).map(|_| ())
    }

    /// Stage content directly in the index, as `git add` would: write each
    /// blob into Git's object store, then point the index entries at them.
    /// `entries` are (path, git mode, content); `removed` are unstaged deletions.
    pub fn stage(
        &self,
        entries: &[(String, String, Vec<u8>)],
        removed: &[String],
        tmp: &Path,
    ) -> Result<()> {
        use std::io::Write;
        // Submodule entries already name their commit; other content is
        // written into Git's object store first.
        let blobs: Vec<&(String, String, Vec<u8>)> =
            entries.iter().filter(|(_, m, _)| m != "160000").collect();
        let mut files = Vec::new();
        for (i, (_, _, content)) in blobs.iter().enumerate() {
            let p = tmp.join(format!("stage-{}-{i}", ghf_proto::id::random_hex(4)));
            fs::write(&p, content)?;
            files.push(p);
        }
        let blob_oids: Vec<String> = if files.is_empty() {
            Vec::new()
        } else {
            let mut child = self
                .git()
                .args(["hash-object", "-w", "--no-filters", "--stdin-paths"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
            let list: String = files.iter().map(|p| format!("{}\n", p.display())).collect();
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(list.as_bytes())?;
            let out = child.wait_with_output()?;
            for f in &files {
                let _ = fs::remove_file(f);
            }
            if !out.status.success() {
                fail!(Internal, "git hash-object failed");
            }
            text(out.stdout).lines().map(str::to_owned).collect()
        };
        let mut blob_oids = blob_oids.into_iter();
        let mut info = String::new();
        let mut oid_len = 40;
        for (path, mode, content) in entries {
            let oid = if mode == "160000" {
                String::from_utf8_lossy(content).trim().to_owned()
            } else {
                blob_oids
                    .next()
                    .ok_or_else(|| err!(Internal, "git hash-object returned too few ids"))?
            };
            oid_len = oid.len();
            info.push_str(&format!("{mode} {oid}\t{path}\n"));
        }
        let zero = "0".repeat(oid_len);
        for path in removed {
            info.push_str(&format!("0 {zero}\t{path}\n"));
        }
        let mut child = self
            .git()
            .args(["update-index", "--index-info"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| err!(Internal, "cannot run git: {e}"))?;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(info.as_bytes())?;
        if !child.wait()?.success() {
            fail!(Internal, "git update-index failed");
        }
        Ok(())
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
    use super::*;

    #[test]
    fn fast_paths_match_git() {
        let dir = std::env::temp_dir().join(format!("ghf-git-{}", ghf_proto::id::random_hex(4)));
        fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str], d: &Path| {
            assert!(
                Command::new("git")
                    .current_dir(d)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@e")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@e")
                    .status()
                    .unwrap()
                    .success()
            );
        };
        g(&["init", "-q", "-b", "main"], &dir);
        let r = Repo::discover(&dir).unwrap();
        assert_eq!(r.head(), None, "unborn");
        g(&["commit", "-q", "--allow-empty", "-m", "x"], &dir);
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        let fast = Repo::discover_fast(&dir.join("sub/deep")).unwrap();
        assert_eq!(fast.head(), fast.resolve_commit("HEAD"));
        g(&["pack-refs", "--all"], &dir);
        assert_eq!(fast.head_fast(), Some(fast.resolve_commit("HEAD")));
        let wt = dir.join("wt");
        g(
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
            &dir,
        );
        let w = Repo::discover_fast(&wt).unwrap();
        assert_eq!(
            w.common_dir,
            crate::platform::canonicalize(dir.join(".git")).unwrap()
        );
        assert_ne!(w.git_dir, w.common_dir);
        assert_eq!(w.head(), fast.head());
        fs::remove_dir_all(dir).unwrap();
    }

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
