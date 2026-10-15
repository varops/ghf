//! Worktree scanning and manifests.
//!
//! A scan lists captured paths (tracked + untracked, minus `.gitignore` and
//! `.ghfignore`), hashes their content, and builds a tree manifest. A stat
//! cache avoids rehashing unchanged files.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use ghf_proto::object::{self, Kind, Mode, TreeEntry};
use ghf_proto::{Change, ChangeStatus, ObjectId, Skipped};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::IGNORE_FILE;
use crate::error::Result;
use crate::git::Repo;
use crate::store::{StatEntry, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub mode: Mode,
    pub id: ObjectId,
    pub size: u64,
}

pub type Manifest = BTreeMap<String, FileEntry>;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub root: ObjectId,
    pub files: Manifest,
    pub skipped: Vec<Skipped>,
    pub bytes: u64,
}

pub struct ScanOptions {
    /// Persist blobs and trees. Without this the scan only hashes, which is
    /// enough to compare against a checkpoint.
    pub persist: bool,
    pub max_file_size: u64,
    /// Stat cache key; `None` disables the cache.
    pub context_id: Option<String>,
}

pub fn ghfignore(root: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    let path = root.join(IGNORE_FILE);
    if path.exists() {
        // A broken pattern is reported by `status`; capture keeps going.
        let _ = b.add(path);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Filter git's listing down to captured paths: drop `.ghfignore` matches,
/// record unsafe and non-UTF-8 paths as skipped.
fn captured(
    ignore: &Gitignore,
    files: Vec<String>,
    other: &[Vec<u8>],
    skipped: &mut Vec<Skipped>,
) -> Vec<String> {
    for _ in other {
        skipped.push(Skipped {
            path: "<non-utf8>".into(),
            reason: "non_utf8_path".into(),
            size: None,
        });
    }
    let mut paths = Vec::with_capacity(files.len());
    for path in files {
        let path = path.trim_end_matches('/').to_owned();
        if !path.split('/').all(ghf_proto::object::safe_name) {
            skipped.push(Skipped {
                path,
                reason: "unsafe_path".into(),
                size: None,
            });
            continue;
        }
        if ignore.matched_path_or_any_parents(&path, false).is_ignore() {
            continue;
        }
        paths.push(path);
    }
    paths
}

/// What one path contributes to a snapshot.
enum Item {
    File(FileEntry, Option<StatEntry>),
    Skip(Skipped),
    Absent,
}

fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.mtime().saturating_mul(1_000_000_000) + md.mtime_nsec()
}

pub fn scan(repo: &Repo, store: &Store, opts: &ScanOptions) -> Result<Snapshot> {
    let started_ns = ghf_proto::id::now_ms().saturating_mul(1_000_000);
    let t = crate::timing::Timer::new();
    let ignore = ghfignore(&repo.root);
    let listing = crate::listing::start(repo)?;
    if std::env::var_os("GHF_TIMING").is_some() {
        eprintln!(
            "ghf timing: listing cache {}",
            if listing.cached() { "hit" } else { "miss" }
        );
    }
    let cache = match &opts.context_id {
        Some(c) => store.load_statcache(c)?,
        None => HashMap::new(),
    };
    t.mark("load stat cache");

    // Hash the last known paths while git refreshes the listing.
    let mut items: HashMap<String, Item> = HashMap::new();
    let mut ignored = Vec::new();
    for path in captured(&ignore, listing.previous().to_vec(), &[], &mut ignored) {
        let item = scan_path(repo, store, opts, &cache, started_ns, &path)?;
        items.insert(path, item);
    }
    t.mark("hash files");
    let (files_listed, other) = listing.finish()?;
    t.mark("list files (wait)");
    let mut skipped = Vec::new();
    let paths = captured(&ignore, files_listed, &other, &mut skipped);

    let mut upserts = Vec::new();
    let mut files = Manifest::new();
    let mut bytes = 0u64;
    for path in paths {
        let item = match items.remove(&path) {
            Some(i) => i,
            None => scan_path(repo, store, opts, &cache, started_ns, &path)?,
        };
        match item {
            Item::File(e, stat) => {
                bytes += e.size;
                if let Some(s) = stat {
                    upserts.push((path.clone(), s));
                }
                files.insert(path, e);
            }
            Item::Skip(s) => skipped.push(s),
            Item::Absent => {}
        }
    }

    if let Some(ctx) = &opts.context_id
        && opts.persist
    {
        let removed: Vec<String> = cache
            .keys()
            .filter(|p| !files.contains_key(*p))
            .cloned()
            .collect();
        store.save_statcache(ctx, &upserts, &removed)?;
    }

    t.mark("reconcile");
    let root = build_tree(&files, opts.persist.then_some(store))?;
    t.mark("build trees");
    Ok(Snapshot {
        root,
        files,
        skipped,
        bytes,
    })
}

/// Stat (and if needed hash and store) one path.
fn scan_path(
    repo: &Repo,
    store: &Store,
    opts: &ScanOptions,
    cache: &HashMap<String, StatEntry>,
    started_ns: i64,
    path: &str,
) -> Result<Item> {
    let skip = |reason: &str, size: Option<u64>| {
        Ok(Item::Skip(Skipped {
            path: path.to_owned(),
            reason: reason.into(),
            size,
        }))
    };
    let full = repo.root.join(path);
    let md = match fs::symlink_metadata(&full) {
        Ok(m) => m,
        // Deleted tracked file: simply absent from this state.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Item::Absent),
        Err(_) => return skip("unreadable", None),
    };
    let ft = md.file_type();
    let mode = if ft.is_symlink() {
        Mode::Symlink
    } else if ft.is_file() {
        if md.mode() & 0o111 != 0 {
            Mode::Exec
        } else {
            Mode::File
        }
    } else if ft.is_dir() {
        // Submodules and nested repositories.
        return skip("submodule", None);
    } else {
        return skip("special_file", None);
    };
    if mode != Mode::Symlink && md.len() > opts.max_file_size {
        return skip("too_large", Some(md.len()));
    }
    let mtime = mtime_ns(&md);
    // Racy-clean protection, as in Git: a file modified within two seconds of
    // the scan is neither trusted from nor written to the stat cache.
    let settled = mtime < started_ns - 2_000_000_000;
    let hit = cache.get(path).filter(|c| {
        settled
            && c.size == md.len()
            && c.mtime == mtime
            && c.ino == md.ino()
            && c.mode == mode
            && (!opts.persist || store.has_object(&c.id))
    });
    let entry = |id| FileEntry {
        mode,
        id,
        size: md.len(),
    };
    if let Some(c) = hit {
        return Ok(Item::File(entry(c.id), None));
    }
    let content = if mode == Mode::Symlink {
        fs::read_link(&full).map(|t| t.as_os_str().as_bytes().to_vec())
    } else {
        fs::read(&full)
    };
    let content = match content {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Item::Absent),
        Err(_) => return skip("unreadable", None),
    };
    let id = if opts.persist {
        store.put(Kind::Blob, &content)?
    } else {
        object::hash(Kind::Blob, &content)
    };
    let stat = (opts.persist && opts.context_id.is_some() && settled).then_some(StatEntry {
        size: md.len(),
        mtime,
        ino: md.ino(),
        mode,
        id,
    });
    Ok(Item::File(entry(id), stat))
}

enum Node<'a> {
    File(&'a FileEntry),
    Dir(BTreeMap<&'a str, Node<'a>>),
}

/// Build (and optionally persist) the tree objects for a manifest.
pub fn build_tree(files: &Manifest, store: Option<&Store>) -> Result<ObjectId> {
    let mut root: BTreeMap<&str, Node<'_>> = BTreeMap::new();
    for (path, entry) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().expect("non-empty path");
        let mut dir = &mut root;
        for p in parts {
            let node = dir.entry(p).or_insert_with(|| Node::Dir(BTreeMap::new()));
            dir = match node {
                Node::Dir(d) => d,
                // A path cannot be both file and directory in one listing.
                Node::File(_) => unreachable!("file/dir collision at {path}"),
            };
        }
        dir.insert(leaf, Node::File(entry));
    }
    fn write(dir: &BTreeMap<&str, Node<'_>>, store: Option<&Store>) -> Result<ObjectId> {
        let mut entries = Vec::with_capacity(dir.len());
        for (name, node) in dir {
            entries.push(match node {
                Node::File(f) => TreeEntry {
                    name: (*name).to_owned(),
                    mode: f.mode,
                    id: f.id,
                    size: f.size,
                },
                Node::Dir(d) => TreeEntry {
                    name: (*name).to_owned(),
                    mode: Mode::Tree,
                    id: write(d, store)?,
                    size: 0,
                },
            });
        }
        let body = object::encode_tree(&entries);
        match store {
            Some(s) => s.put(Kind::Tree, &body),
            None => Ok(object::hash(Kind::Tree, &body)),
        }
    }
    write(&root, store)
}

/// Flatten a stored manifest tree into path -> entry.
pub fn flatten(store: &Store, root: &ObjectId) -> Result<Manifest> {
    let mut out = Manifest::new();
    fn walk(store: &Store, id: &ObjectId, prefix: &str, out: &mut Manifest) -> Result<()> {
        for e in store.tree(id)? {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.mode == Mode::Tree {
                walk(store, &e.id, &path, out)?;
            } else {
                out.insert(
                    path,
                    FileEntry {
                        mode: e.mode,
                        id: e.id,
                        size: e.size,
                    },
                );
            }
        }
        Ok(())
    }
    walk(store, root, "", &mut out)?;
    Ok(out)
}

/// A path whose entry differs between two manifests.
#[derive(Debug, Clone)]
pub struct Delta {
    pub path: String,
    pub old: Option<FileEntry>,
    pub new: Option<FileEntry>,
}

/// Diff two stored trees, skipping identical subtrees.
pub fn diff_trees(store: &Store, a: Option<&ObjectId>, b: Option<&ObjectId>) -> Result<Vec<Delta>> {
    let mut out = Vec::new();
    fn entries(store: &Store, id: Option<&ObjectId>) -> Result<BTreeMap<String, TreeEntry>> {
        Ok(match id {
            Some(id) => store
                .tree(id)?
                .into_iter()
                .map(|e| (e.name.clone(), e))
                .collect(),
            None => BTreeMap::new(),
        })
    }
    fn file(e: &TreeEntry) -> FileEntry {
        FileEntry {
            mode: e.mode,
            id: e.id,
            size: e.size,
        }
    }
    fn walk(
        store: &Store,
        a: Option<&ObjectId>,
        b: Option<&ObjectId>,
        prefix: &str,
        out: &mut Vec<Delta>,
    ) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let ea = entries(store, a)?;
        let eb = entries(store, b)?;
        let mut names: Vec<&String> = ea.keys().chain(eb.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let (x, y) = (ea.get(name), eb.get(name));
            if x.map(|e| (e.mode, e.id)) == y.map(|e| (e.mode, e.id)) {
                continue;
            }
            let xt = x.filter(|e| e.mode == Mode::Tree);
            let yt = y.filter(|e| e.mode == Mode::Tree);
            if xt.is_some() || yt.is_some() {
                walk(store, xt.map(|e| &e.id), yt.map(|e| &e.id), &path, out)?;
            }
            let xf = x.filter(|e| e.mode != Mode::Tree).map(file);
            let yf = y.filter(|e| e.mode != Mode::Tree).map(file);
            if xf != yf {
                out.push(Delta {
                    path: path.clone(),
                    old: xf,
                    new: yf,
                });
            }
        }
        Ok(())
    }
    walk(store, a, b, "", &mut out)?;
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

/// Diff two in-memory manifests.
pub fn diff_manifests(a: &Manifest, b: &Manifest) -> Vec<Delta> {
    let mut out = Vec::new();
    for (path, x) in a {
        match b.get(path) {
            Some(y) if y == x => {}
            y => out.push(Delta {
                path: path.clone(),
                old: Some(*x),
                new: y.copied(),
            }),
        }
    }
    for (path, y) in b {
        if !a.contains_key(path) {
            out.push(Delta {
                path: path.clone(),
                old: None,
                new: Some(*y),
            });
        }
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    out
}

/// Summarize deltas as changes, pairing identical delete+add as renames.
pub fn changes(deltas: &[Delta]) -> Vec<Change> {
    let mut added: HashMap<ObjectId, Vec<usize>> = HashMap::new();
    for (i, d) in deltas.iter().enumerate() {
        if let (None, Some(n)) = (&d.old, &d.new) {
            added.entry(n.id).or_default().push(i);
        }
    }
    let mut renamed_to: HashMap<usize, String> = HashMap::new();
    for d in deltas {
        if let (Some(o), None) = (&d.old, &d.new)
            && let Some(slot) = added.get_mut(&o.id)
            && let Some(i) = slot.pop()
        {
            renamed_to.insert(i, d.path.clone());
        }
    }
    let renamed_from: std::collections::HashSet<&String> = renamed_to.values().collect();
    let mut out = Vec::new();
    for (i, d) in deltas.iter().enumerate() {
        let (status, from) = match (&d.old, &d.new) {
            (None, Some(_)) => match renamed_to.get(&i) {
                Some(f) => (ChangeStatus::Renamed, Some(f.clone())),
                None => (ChangeStatus::Added, None),
            },
            (Some(_), None) if renamed_from.contains(&d.path) => continue,
            (Some(_), None) => (ChangeStatus::Deleted, None),
            (Some(o), Some(n)) if o.id == n.id => (ChangeStatus::Mode, None),
            _ => (ChangeStatus::Modified, None),
        };
        out.push(Change {
            path: d.path.clone(),
            status,
            from,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe(n: u8) -> FileEntry {
        FileEntry {
            mode: Mode::File,
            id: object::hash(Kind::Blob, &[n]),
            size: 1,
        }
    }

    #[test]
    fn detects_renames() {
        let mut a = Manifest::new();
        a.insert("old.txt".into(), fe(1));
        a.insert("keep.txt".into(), fe(2));
        let mut b = Manifest::new();
        b.insert("new.txt".into(), fe(1));
        b.insert("keep.txt".into(), fe(3));
        let ch = changes(&diff_manifests(&a, &b));
        assert_eq!(ch.len(), 2);
        assert!(ch.iter().any(|c| c.status == ChangeStatus::Renamed
            && c.path == "new.txt"
            && c.from.as_deref() == Some("old.txt")));
        assert!(
            ch.iter()
                .any(|c| c.status == ChangeStatus::Modified && c.path == "keep.txt")
        );
    }

    #[test]
    fn tree_hash_is_stable() {
        let mut a = Manifest::new();
        a.insert("d/x".into(), fe(1));
        a.insert("y".into(), fe(2));
        assert_eq!(
            build_tree(&a, None).unwrap(),
            build_tree(&a.clone(), None).unwrap()
        );
        a.insert("d/z".into(), fe(3));
        assert_ne!(
            build_tree(&a, None).unwrap(),
            build_tree(&Manifest::new(), None).unwrap()
        );
    }
}
