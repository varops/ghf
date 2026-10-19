//! Cached `git ls-files` listing.
//!
//! Spawning git dominates capture latency, and an agent's typical edit
//! changes file contents, not the set of files. So the listing is cached per
//! worktree together with stamps (mtime, size, inode) of every directory that
//! holds a listed file, the index, and the ignore files. Creating, deleting
//! or renaming a file changes its directory's mtime, so a matching set of
//! stamps means git would return the same list. This is the idea behind
//! Git's own untracked cache, kept in ghf's directory instead of the index.
//!
//! Safety rules:
//! - Directories are stamped *before* git runs. A change after that makes the
//!   stamp stale, which forces a refresh next time.
//! - A stamp within two seconds of the listing time is racy (a same-tick
//!   change could keep the same mtime) and is never trusted.
//! - The cache expires after `MAX_AGE`. This bounds staleness for changes no
//!   stamp sees, such as a new file in a directory that so far held only
//!   ignored files, or an edited global excludes file.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::git::Repo;
use crate::workspace::ghf_dir;

const VERSION: u32 = 1;
const MAX_AGE_NS: i64 = 60 * 1_000_000_000;
const RACY_NS: i64 = 2_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    /// Nanoseconds; -1 when the path did not exist.
    mtime: i64,
    size: u64,
    ino: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    version: u32,
    listed_at: i64,
    /// UTF-8 paths, sorted, as git listed them.
    files: Vec<String>,
    /// Raw paths that are not UTF-8 (kept so scan can report them).
    other: Vec<Vec<u8>>,
    /// Absolute path -> stamp. `None` marks a racy or unknown stamp.
    stamps: HashMap<PathBuf, Option<Stamp>>,
}

fn now_ns() -> i64 {
    ghf_proto::id::now_ms().saturating_mul(1_000_000)
}

fn stamp(path: &Path) -> Stamp {
    match fs::symlink_metadata(path) {
        Ok(md) => Stamp {
            mtime: crate::platform::mtime_ns(&md),
            size: md.len(),
            ino: crate::platform::file_id(&md),
        },
        Err(_) => Stamp {
            mtime: -1,
            size: 0,
            ino: 0,
        },
    }
}

fn cache_path(repo: &Repo) -> PathBuf {
    ghf_dir(repo).join("listing.json")
}

/// Files outside the worktree whose change can alter git's answer.
fn sentinels(repo: &Repo) -> Vec<PathBuf> {
    let mut v = vec![
        repo.git_dir.join("index"),
        repo.common_dir.join("info/exclude"),
        repo.common_dir.join("config"),
    ];
    if let Some(home) = crate::platform::home_dir() {
        v.push(home.join(".gitconfig"));
        v.push(home.join(".gitignore"));
        v.push(home.join(".config/git/ignore"));
        v.push(home.join(".config/git/config"));
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        v.push(PathBuf::from(&x).join("git/ignore"));
        v.push(PathBuf::from(x).join("git/config"));
    }
    v
}

/// Worktree directories holding listed files, plus `.gitignore` files.
fn watched(root: &Path, files: &[String]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    out.insert(root.to_path_buf());
    for f in files {
        if f.rsplit('/').next() == Some(".gitignore") {
            out.insert(root.join(f));
        }
        let mut dir = f.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            if !out.insert(root.join(parent)) {
                break;
            }
            dir = parent;
        }
    }
    out
}

impl Cache {
    fn valid(&self, now: i64) -> bool {
        self.version == VERSION
            && now - self.listed_at < MAX_AGE_NS
            && now >= self.listed_at
            && self
                .stamps
                .iter()
                .all(|(p, s)| s.is_some_and(|s| stamp(p) == s))
    }
}

/// UTF-8 paths and raw non-UTF-8 paths, as git listed them.
pub type Paths = (Vec<String>, Vec<Vec<u8>>);

/// A listing that is either ready or being refreshed by git on a background
/// thread. While git runs, the caller can start on `previous`, the last known
/// list, which usually overlaps the new one almost entirely.
pub enum Listing {
    Ready {
        files: Vec<String>,
        other: Vec<Vec<u8>>,
    },
    Pending {
        previous: Vec<String>,
        handle: std::thread::JoinHandle<Result<Paths>>,
    },
}

impl Listing {
    pub fn cached(&self) -> bool {
        matches!(self, Listing::Ready { .. })
    }

    /// Paths worth starting on before the listing is final.
    pub fn previous(&self) -> &[String] {
        match self {
            Listing::Ready { files, .. } => files,
            Listing::Pending { previous, .. } => previous,
        }
    }

    /// UTF-8 paths and raw non-UTF-8 ones, as git lists them.
    pub fn finish(self) -> Result<Paths> {
        match self {
            Listing::Ready { files, other } => Ok((files, other)),
            Listing::Pending { handle, .. } => handle
                .join()
                .unwrap_or_else(|_| Err(crate::err!(Internal, "file listing thread panicked"))),
        }
    }
}

/// Start listing captured-path candidates: tracked plus untracked, minus
/// ignored, exactly as `git ls-files` reports them.
pub fn start(repo: &Repo) -> Result<Listing> {
    if std::env::var_os("GHF_NO_LISTING_CACHE").is_some() {
        let (files, other) = split(repo.list_files()?);
        return Ok(Listing::Ready { files, other });
    }
    let path = cache_path(repo);
    let old: Option<Cache> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(c) = old.as_ref().filter(|c| c.valid(now_ns())) {
        return Ok(Listing::Ready {
            files: c.files.clone(),
            other: c.other.clone(),
        });
    }
    let (previous, watched_before) = match old {
        Some(c) => (c.files, c.stamps.into_keys().collect()),
        None => (Vec::new(), Vec::new()),
    };
    let repo = repo.clone();
    let handle = std::thread::spawn(move || refresh(&repo, watched_before));
    Ok(Listing::Pending { previous, handle })
}

/// Ask git, then record the listing with stamps taken *before* git ran.
fn refresh(repo: &Repo, watched_before: Vec<PathBuf>) -> Result<Paths> {
    let mut pre: HashMap<PathBuf, Stamp> = HashMap::new();
    for p in watched_before.into_iter().chain(sentinels(repo)) {
        let s = stamp(&p);
        pre.insert(p, s);
    }
    let listed_at = now_ns();
    let (files, other) = split(repo.list_files()?);

    let mut stamps = HashMap::new();
    for p in watched(&repo.root, &files)
        .into_iter()
        .chain(sentinels(repo))
    {
        let s = pre
            .get(&p)
            .copied()
            .filter(|s| s.mtime < listed_at - RACY_NS);
        stamps.insert(p, s);
    }
    let cache = Cache {
        version: VERSION,
        listed_at,
        files,
        other,
        stamps,
    };
    // The cache is an optimization; failing to write it is not an error.
    let path = cache_path(repo);
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let tmp = path.with_extension(format!("tmp-{}", ghf_proto::id::random_hex(4)));
        if fs::create_dir_all(ghf_dir(repo)).is_ok() && fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
    Ok((cache.files, cache.other))
}

/// Synchronous [`start`] + [`Listing::finish`]; also reports cache use.
#[cfg(test)]
pub fn list(repo: &Repo) -> Result<(Vec<String>, Vec<Vec<u8>>, bool)> {
    let l = start(repo)?;
    let cached = l.cached();
    let (files, other) = l.finish()?;
    Ok((files, other, cached))
}

fn split(raw: Vec<Vec<u8>>) -> Paths {
    let mut files = Vec::with_capacity(raw.len());
    let mut other = Vec::new();
    for p in raw {
        match String::from_utf8(p) {
            Ok(s) => files.push(s),
            Err(e) => other.push(e.into_bytes()),
        }
    }
    (files, other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    fn age(path: &Path, secs: u64) {
        crate::platform::set_times(path, SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    /// Refresh the listing with every stamp under `dir` aged past the racy
    /// window, so the next call can reuse it. Only paths inside the test
    /// directory are touched.
    fn settle(repo: &Repo, dir: &Path) {
        list(repo).unwrap();
        let c: Cache = serde_json::from_slice(&fs::read(cache_path(repo)).unwrap()).unwrap();
        for p in c.stamps.keys() {
            if p.starts_with(dir) && p.exists() {
                age(p, 10);
            }
        }
        list(repo).unwrap();
    }

    #[test]
    fn reuses_listing_until_the_file_set_changes() {
        let dir = std::env::temp_dir().join(format!("ghf-list-{}", ghf_proto::id::random_hex(4)));
        fs::create_dir_all(dir.join("a/b")).unwrap();
        let dir = crate::platform::canonicalize(dir).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .current_dir(&dir)
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.join("a/b/one.txt"), "1").unwrap();
        fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        let repo = Repo::discover(&dir).unwrap();

        let (files, _, cached) = list(&repo).unwrap();
        assert!(!cached);
        assert!(files.contains(&"a/b/one.txt".to_owned()));
        // Fresh stamps are racy, so the next call asks git again.
        assert!(!list(&repo).unwrap().2);

        settle(&repo, &dir);
        let (again, _, cached) = list(&repo).unwrap();
        assert!(cached, "unchanged tree reuses the listing");
        assert_eq!(again, files);

        // Editing content keeps the cache; adding a file invalidates it.
        fs::write(dir.join("a/b/one.txt"), "changed").unwrap();
        assert!(list(&repo).unwrap().2);
        fs::write(dir.join("a/b/two.txt"), "2").unwrap();
        let (files, _, cached) = list(&repo).unwrap();
        assert!(!cached);
        assert!(files.contains(&"a/b/two.txt".to_owned()));

        // So do deleting a file and changing ignore rules.
        settle(&repo, &dir);
        assert!(list(&repo).unwrap().2);
        fs::remove_file(dir.join("a/b/one.txt")).unwrap();
        assert!(!list(&repo).unwrap().2);
        settle(&repo, &dir);
        assert!(list(&repo).unwrap().2);
        fs::write(dir.join(".gitignore"), "*.log\ntwo.txt\n").unwrap();
        let (files, _, cached) = list(&repo).unwrap();
        assert!(!cached);
        assert!(!files.contains(&"a/b/two.txt".to_owned()));
        fs::remove_dir_all(dir).unwrap();
    }
}
