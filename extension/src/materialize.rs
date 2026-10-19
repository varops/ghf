//! Write a manifest's state onto a worktree.
//!
//! Only captured paths are touched: files ignored by `.gitignore` or
//! `.ghfignore`, skipped files and anything else not in the current scan are
//! never written, replaced or deleted. If the target needs such a path, the
//! whole operation is refused before anything changes.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ghf_proto::object::Mode;
use serde::Serialize;
use serde_json::json;

use crate::error::{Context, Result};
use crate::scan::Manifest;
use crate::store::Store;
use crate::{err, fail};

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub written: usize,
    pub deleted: usize,
    /// Submodules that could not be moved to the recorded commit.
    pub warnings: Vec<String>,
}

/// Move a submodule to `sha`, but only when that is safe: it is checked
/// out, has no uncommitted changes, and has the commit locally. Returns a
/// warning instead of acting otherwise; never fetches.
fn checkout_submodule(root: &Path, path: &str, sha: &str) -> Option<String> {
    let dir = root.join(path);
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
    };
    if crate::scan::submodule_head(&dir).is_none() {
        return Some(format!(
            "submodule {path} is not checked out; run `git submodule update --init {path}`, then `git -C {path} checkout {sha}`"
        ));
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_none_or(|o| !o.status.success() || !o.stdout.is_empty());
    if dirty {
        return Some(format!(
            "submodule {path} has uncommitted changes; left as is (checkpoint had {sha})"
        ));
    }
    let have =
        git(&["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some_and(|o| o.status.success());
    if !have {
        return Some(format!(
            "submodule {path} does not have commit {sha}; fetch it, then `git -C {path} checkout {sha}`"
        ));
    }
    match git(&["checkout", "-q", "--detach", sha]) {
        Some(o) if o.status.success() => None,
        _ => Some(format!("submodule {path}: checkout of {sha} failed")),
    }
}

/// Make the captured state of the worktree at `root` equal `target`.
/// `current` is a fresh scan of the worktree.
pub fn materialize(
    root: &Path,
    store: &Store,
    target: &Manifest,
    current: &Manifest,
) -> Result<Stats> {
    // Fail before touching anything if content is missing.
    for (path, e) in target {
        if current.get(path) != Some(e) && !store.has_object(&e.id) {
            fail!(
                ObjectMissing,
                "content for {path} ({}) is not available locally",
                e.id.short()
            );
        }
    }

    let blocked = blockers(root, target, current);
    if !blocked.is_empty() {
        return Err(err!(
            WorktreeDirty,
            "{} path(s) hold files ghf did not capture (ignored, skipped or untracked); move them or restore into a new worktree: {}",
            blocked.len(),
            blocked.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        )
        .with_data(json!({"paths": blocked})));
    }

    let mut stats = Stats::default();
    let mut touched_dirs = BTreeSet::new();
    for (path, e) in current.iter().filter(|(p, _)| !target.contains_key(*p)) {
        if e.mode == Mode::Gitlink {
            stats.warnings.push(format!(
                "submodule {path} is not in the checkpoint; left in place"
            ));
            continue;
        }
        let full = root.join(path);
        match fs::remove_file(&full) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(|| format!("delete {path}")),
        }
        if let Some(p) = full.parent() {
            touched_dirs.insert(p.to_path_buf());
        }
    }

    for (path, e) in target {
        if current.get(path) == Some(e) {
            continue;
        }
        if e.mode == Mode::Gitlink {
            let sha = String::from_utf8_lossy(&store.blob(&e.id)?).into_owned();
            if let Some(w) = checkout_submodule(root, path, &sha) {
                stats.warnings.push(w);
            } else {
                stats.written += 1;
            }
            continue;
        }
        let full = root.join(path);
        prepare_parents(root, path, current)?;
        if fs::symlink_metadata(&full).is_ok_and(|md| md.is_dir()) && fs::remove_dir(&full).is_err()
        {
            fail!(
                WorktreeDirty,
                "cannot replace directory {path} with a file: it still holds uncaptured files"
            );
        }
        let content = store.blob(&e.id)?;
        match e.mode {
            Mode::Symlink => {
                use std::os::unix::ffi::OsStrExt;
                let _ = fs::remove_file(&full);
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&content), &full)
                    .context(|| format!("create symlink {path}"))?;
            }
            Mode::File | Mode::Exec => {
                let dir = full.parent().expect("file has parent");
                let tmp = dir.join(format!(".ghf-tmp-{}", ghf_proto::id::random_hex(6)));
                fs::write(&tmp, &content).context(|| format!("write {path}"))?;
                let perm = if e.mode == Mode::Exec { 0o755 } else { 0o644 };
                fs::set_permissions(&tmp, fs::Permissions::from_mode(perm))?;
                if fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
                    fs::remove_file(&full)?;
                }
                fs::rename(&tmp, &full).context(|| format!("write {path}"))?;
            }
            Mode::Tree | Mode::Gitlink => unreachable!("handled above"),
        }
        stats.written += 1;
    }

    // Remove directories emptied by deletions, deepest first.
    for dir in touched_dirs.iter().rev() {
        let mut d = dir.as_path();
        while d != root && d.starts_with(root) {
            if fs::remove_dir(d).is_err() {
                break;
            }
            match d.parent() {
                Some(p) => d = p,
                None => break,
            }
        }
    }
    Ok(stats)
}

/// Paths where writing `target` would clobber something not captured in
/// `current`: an uncaptured file at a target path, an uncaptured file where a
/// target needs a directory, or a directory holding uncaptured files where a
/// target needs a file. Also reports targets that collide with each other.
pub fn blockers(root: &Path, target: &Manifest, current: &Manifest) -> Vec<String> {
    let mut out = BTreeSet::new();
    for (path, e) in target {
        let unchanged = current.get(path) == Some(e);
        let mut prefix = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if target.contains_key(&prefix) {
                out.insert(prefix.clone());
                continue;
            }
            if !unchanged
                && let Ok(md) = fs::symlink_metadata(root.join(&prefix))
                && !md.is_dir()
                && !current.contains_key(&prefix)
            {
                out.insert(prefix.clone());
            }
        }
        if unchanged || current.contains_key(path) || e.mode == Mode::Gitlink {
            continue;
        }
        match fs::symlink_metadata(root.join(path)) {
            Ok(md) if md.is_dir() => {
                if holds_uncaptured(root, path, current) {
                    out.insert(path.clone());
                }
            }
            Ok(_) => {
                out.insert(path.clone());
            }
            Err(_) => {}
        }
    }
    out.into_iter().collect()
}

fn holds_uncaptured(root: &Path, rel: &str, current: &Manifest) -> bool {
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        return true;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if if is_dir {
            holds_uncaptured(root, &child, current)
        } else {
            !current.contains_key(&child)
        } {
            return true;
        }
    }
    false
}

/// Ensure every ancestor of `path` is a directory, removing captured files
/// that stand in the way.
fn prepare_parents(root: &Path, path: &str, current: &Manifest) -> Result<()> {
    let mut rel = String::new();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        if !rel.is_empty() {
            rel.push('/');
        }
        rel.push_str(part);
        let full = root.join(&rel);
        match fs::symlink_metadata(&full) {
            Ok(md) if md.is_dir() => continue,
            Ok(_) => {
                if !current.contains_key(&rel) {
                    fail!(
                        WorktreeDirty,
                        "{rel} is an uncaptured file where {path} needs a directory"
                    );
                }
                fs::remove_file(&full)?;
                fs::create_dir(&full)?;
            }
            Err(_) => fs::create_dir_all(&full).context(|| format!("create {rel}"))?,
        }
    }
    Ok(())
}
