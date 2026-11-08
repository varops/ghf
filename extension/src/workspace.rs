//! A ghf-enabled worktree: repository, configuration, local store and
//! worktree context.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{self, RepoConfig};
use crate::error::{Context, Result};
use crate::fail;
use crate::git::Repo;
use crate::store::Store;

/// Identity of one working directory on one machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeContext {
    pub id: String,
    pub path: PathBuf,
    pub machine_id: String,
    pub created_at: i64,
}

pub struct Workspace {
    pub repo: Repo,
    pub cfg: RepoConfig,
    pub store: Store,
    pub ctx: WorktreeContext,
    /// Set when this invocation replaced a context copied from elsewhere.
    pub replaced_context: Option<String>,
}

pub fn ghf_dir(repo: &Repo) -> PathBuf {
    repo.git_dir.join("ghf")
}

/// The id in the context file under `git_dir`, if there is one.
pub fn context_id_at(git_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(git_dir.join("ghf").join("context.json")).ok()?;
    serde_json::from_str::<WorktreeContext>(&text)
        .ok()
        .map(|c| c.id)
}

impl Workspace {
    /// Open the worktree containing `cwd`. Fails with `E_NOT_INITIALIZED`
    /// when the repository has no `.ghf`.
    pub fn open(cwd: &Path) -> Result<Self> {
        let repo = Repo::discover(cwd)?;
        let Some(cfg) = RepoConfig::load(&repo.root)? else {
            fail!(
                NotInitialized,
                "no {} in {}; run `git hf init`",
                config::FILE_NAME,
                repo.root.display()
            );
        };
        Self::with_config(repo, cfg)
    }

    pub fn with_config(repo: Repo, cfg: RepoConfig) -> Result<Self> {
        let store = Store::open(&repo.common_dir)?;
        let (ctx, created, replaced_context) = load_or_create_context(&repo)?;
        if created {
            store.tx(|| {
                store.note_context(&ctx.id, &repo.git_dir)?;
                // The old context is gone for good: nothing will write to
                // its open trace again.
                if let Some(old) = &replaced_context {
                    store.close_context_traces(old, ghf_proto::id::now_ms())?;
                    store.forget_context(old)?;
                }
                Ok(())
            })?;
        }
        Ok(Self {
            repo,
            cfg,
            store,
            ctx,
            replaced_context,
        })
    }

    pub fn lock_path(&self, name: &str) -> PathBuf {
        ghf_dir(&self.repo).join(name)
    }
}

fn context_path(repo: &Repo) -> PathBuf {
    ghf_dir(repo).join("context.json")
}

/// Load this worktree's context, or create one. A context whose recorded
/// path or machine differs (a copied directory or cloned VM) is replaced so
/// two live contexts never share an id. Returns the context, whether it was
/// created, and the id it replaced.
fn load_or_create_context(repo: &Repo) -> Result<(WorktreeContext, bool, Option<String>)> {
    let path = context_path(repo);
    let machine_id = config::machine_id()?;
    let root = crate::platform::canonicalize(&repo.root).unwrap_or_else(|_| repo.root.clone());
    let mut replaced = None;
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(ctx) = serde_json::from_str::<WorktreeContext>(&text)
    {
        if ctx.path == root && ctx.machine_id == machine_id {
            return Ok((ctx, false, None));
        }
        replaced = Some(ctx.id);
    }
    let ctx = new_context(repo, &root, machine_id)?;
    Ok((ctx, true, replaced))
}

pub fn new_context(repo: &Repo, root: &Path, machine_id: String) -> Result<WorktreeContext> {
    let ctx = WorktreeContext {
        id: ghf_proto::id::new_id(ghf_proto::id::CONTEXT),
        path: root.to_path_buf(),
        machine_id,
        created_at: ghf_proto::id::now_ms(),
    };
    let dir = ghf_dir(repo);
    fs::create_dir_all(&dir).context(|| format!("create {}", dir.display()))?;
    let path = context_path(repo);
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&ctx)?.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(ctx)
}

/// An in-place restore or apply that started changing the working tree.
/// Its journal file exists from just before the tree changes until the
/// result is recorded; if it is still there, the operation was interrupted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interrupted {
    /// `restore` or `apply`.
    pub operation: String,
    /// The checkpoint restored, or the `A..B` range applied.
    pub target: String,
    /// The checkpoint holding the working tree from before the operation.
    pub undo: Option<String>,
    pub started_at: i64,
}

impl Interrupted {
    pub fn undo_command(&self) -> Option<String> {
        self.undo
            .as_ref()
            .map(|u| format!("git hf trace restore {u} --in-place --force"))
    }
}

impl Workspace {
    fn journal_path(&self) -> PathBuf {
        self.lock_path("operation.json")
    }

    /// Record, durably, that `operation` is about to change the tree.
    pub fn begin_operation(&self, operation: &str, target: &str, undo: Option<&str>) -> Result<()> {
        let j = Interrupted {
            operation: operation.to_owned(),
            target: target.to_owned(),
            undo: undo.map(str::to_owned),
            started_at: ghf_proto::id::now_ms(),
        };
        let path = self.journal_path();
        let tmp = path.with_extension("tmp");
        let mut f = fs::File::create(&tmp).context(|| format!("create {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(&j)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The operation finished: its result is recorded.
    pub fn end_operation(&self) -> Result<()> {
        match fs::remove_file(self.journal_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The in-place operation that was interrupted in this worktree, if any.
    pub fn interrupted(&self) -> Option<Interrupted> {
        let text = fs::read_to_string(self.journal_path()).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Exclusive advisory lock held for the life of the guard.
pub struct FileLock(#[allow(dead_code)] fs::File);

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .context(|| format!("open lock {}", path.display()))?;
        f.lock().context(|| format!("lock {}", path.display()))?;
        Ok(Self(f))
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .context(|| format!("open lock {}", path.display()))?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Self(f))),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// The writer identity for this process: `$GHF_WRITER`, else `manual:<user>`.
pub fn default_writer() -> String {
    if let Ok(w) = std::env::var("GHF_WRITER")
        && !w.is_empty()
    {
        return w;
    }
    let user = crate::platform::user_name();
    format!("manual:{user}")
}
