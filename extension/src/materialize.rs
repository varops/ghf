//! Write a manifest's state onto a worktree.
//!
//! Only captured paths are touched: files ignored by `.gitignore` or
//! `.ghfignore` are never written or deleted.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ghf_proto::object::Mode;
use serde::Serialize;

use crate::error::{Context, Result};
use crate::fail;
use crate::scan::Manifest;
use crate::store::Store;

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub written: usize,
    pub deleted: usize,
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

    let mut stats = Stats::default();
    let mut touched_dirs = BTreeSet::new();
    for path in current.keys().filter(|p| !target.contains_key(*p)) {
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
            Mode::Tree => unreachable!("manifests hold files only"),
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
