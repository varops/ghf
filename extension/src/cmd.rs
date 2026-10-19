//! Command implementations. Each returns JSON data plus human text.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ghf_proto::object::Mode;
use ghf_proto::retention::{self, DAY_MS};
use ghf_proto::{CheckpointRecord, Hold, Policy, TraceRecord, TraceState};
use serde_json::{Value, json};

use crate::capture::{self, Request};
use crate::config::{self, RepoConfig, ServerConfig};
use crate::error::{Context, Result};
use crate::git::{self, Repo};
use crate::materialize;
use crate::refs;
use crate::remote::{self, Remote};
use crate::scan::{self, FileEntry, Manifest};
use crate::store::TraceRow;
use crate::workspace::{self, Workspace};
use crate::{err, fail};

pub struct Out {
    pub data: Value,
    pub human: String,
}

impl Out {
    fn new(data: Value, human: impl Into<String>) -> Self {
        Self {
            data,
            human: human.into(),
        }
    }
}

pub struct Env {
    pub cwd: PathBuf,
    pub json: bool,
}

impl Env {
    fn prompt(&self) -> bool {
        remote::interactive(self.json)
    }

    /// Open the workspace. Non-capture commands also tidy the local store.
    fn ws(&self) -> Result<Workspace> {
        let ws = Workspace::open(&self.cwd)?;
        ws.store.maintain(4 * 1024 * 1024);
        Ok(ws)
    }
}

fn fmt_time(ms: i64) -> String {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64);
    humantime::format_rfc3339_seconds(t).to_string()
}

fn trace_json(ws: &Workspace, t: &TraceRow, policy: &Policy) -> Result<Value> {
    let count = ws.store.checkpoint_count(&t.record.id)?;
    let unsynced = ws.store.unsynced_in_trace(&t.record.id)?;
    let el = retention::eligibility(&t.record, ghf_proto::id::now_ms(), policy);
    let mut v = serde_json::to_value(&t.record)?;
    let o = v.as_object_mut().expect("object");
    o.insert("checkpoints".into(), json!(count));
    o.insert("unsynced".into(), json!(unsynced));
    o.insert("writer".into(), json!(t.writer));
    o.insert("local".into(), json!(t.record.context_id == ws.ctx.id));
    o.insert("retention".into(), serde_json::to_value(el)?);
    Ok(v)
}

fn policy(ws: &Workspace) -> Result<Policy> {
    if let Some(p) = ws.store.kv_get("server_policy")?
        && let Ok(p) = serde_json::from_str(&p)
    {
        return Ok(p);
    }
    ws.cfg.policy()
}

// ---------------------------------------------------------------------------
// init, trust, login

pub struct InitArgs {
    pub server: Option<String>,
    pub repository: Option<String>,
    pub strict: bool,
    pub trust: bool,
    pub force: bool,
}

pub fn init(env: &Env, a: InitArgs) -> Result<Out> {
    let repo = Repo::discover(&env.cwd)?;
    let path = repo.root.join(config::FILE_NAME);
    let existing = RepoConfig::load(&repo.root)?;
    let wants_change = a.server.is_some() || a.repository.is_some() || a.strict;
    let (cfg, created) = match existing {
        Some(c) if !wants_change || !a.force => {
            if wants_change {
                fail!(
                    PathExists,
                    "{} already exists; pass --force to overwrite it",
                    path.display()
                );
            }
            (c, false)
        }
        _ => {
            let server = match a.server {
                Some(url) => {
                    let url = config::normalize_url(&url)?;
                    let repository = match a.repository {
                        Some(r) => r,
                        None => repo
                            .remote_url("origin")
                            .and_then(|u| git::repo_name_from_url(&u))
                            .ok_or_else(|| {
                                err!(Usage, "cannot derive owner/name from the origin remote; pass --repository")
                            })?,
                    };
                    if !ghf_proto::api::valid_repo(&repository) {
                        fail!(Usage, "--repository must look like owner/name");
                    }
                    Some(ServerConfig { url, repository })
                }
                None => None,
            };
            let mut cfg = RepoConfig {
                version: 1,
                server,
                ..Default::default()
            };
            if a.strict {
                cfg.capture.mode = Some("strict".into());
            }
            let text = format!(
                "# ghf configuration. Safe to commit: holds no credentials.\n# See https://github.com/varops/ghf\n{}",
                cfg.to_toml()
            );
            fs::write(&path, text).context(|| format!("write {}", path.display()))?;
            (cfg, true)
        }
    };
    let ws = Workspace::with_config(repo, cfg.clone())?;
    let mut trusted = false;
    if let Some(s) = &cfg.server {
        let url = config::normalize_url(&s.url)?;
        if config::trusted(&url)?.is_some() {
            trusted = true;
        } else if a.trust {
            let info = remote::fetch_info(&url)?;
            config::trust(&url, &info.server_id)?;
            trusted = true;
        } else if env.prompt() {
            trusted = Remote::connect(&ws, true).is_ok();
        }
    }
    let mut human = if created {
        format!("wrote {}\n", path.display())
    } else {
        format!("{} already present\n", path.display())
    };
    match &cfg.server {
        None => human.push_str(
            "no server configured: checkpoints stay on this machine (add one with `git hf init --server URL --force`)\n",
        ),
        Some(s) if !trusted => human.push_str(&format!(
            "server {} is not trusted yet; run `git hf trust` before syncing\n",
            s.url
        )),
        Some(s) => human.push_str(&format!("server {} trusted\n", s.url)),
    }
    human.push_str(&format!("worktree context {}", ws.ctx.id));
    Ok(Out::new(
        json!({
            "path": path,
            "created": created,
            "config": cfg,
            "context": ws.ctx.id,
            "trusted": trusted,
        }),
        human,
    ))
}

pub fn trust(env: &Env, url: Option<String>) -> Result<Out> {
    let url = match url {
        Some(u) => config::normalize_url(&u)?,
        None => {
            let ws = env.ws()?;
            let s = ws
                .cfg
                .server
                .as_ref()
                .ok_or_else(|| err!(Usage, "no server in .ghf; pass a URL"))?;
            config::normalize_url(&s.url)?
        }
    };
    let info = remote::fetch_info(&url)?;
    config::trust(&url, &info.server_id)?;
    Ok(Out::new(
        json!({"url": url, "server_id": info.server_id, "version": info.version}),
        format!("trusted {url} (server id {})", info.server_id),
    ))
}

pub fn login(env: &Env, server: Option<String>) -> Result<Out> {
    let url = match server {
        Some(u) => config::normalize_url(&u)?,
        None => {
            let ws = env.ws()?;
            let s = ws
                .cfg
                .server
                .as_ref()
                .ok_or_else(|| err!(Usage, "no server in .ghf; pass --server"))?;
            config::normalize_url(&s.url)?
        }
    };
    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim();
    if token.is_empty() {
        fail!(
            Usage,
            "pipe the token on stdin, e.g. `echo $TOKEN | git hf login`"
        );
    }
    config::save_token(&url, token)?;
    Ok(Out::new(
        json!({"url": url, "saved": true}),
        format!("saved credentials for {url}"),
    ))
}

// ---------------------------------------------------------------------------
// status

pub fn status(env: &Env) -> Result<Out> {
    let ws = env.ws()?;
    let now = ghf_proto::id::now_ms();
    let policy = policy(&ws)?;
    let mut human = Vec::new();
    let mut warnings = Vec::new();

    let open = ws.store.open_trace(&ws.ctx.id)?;
    let trace = match &open {
        Some(t) => Some(trace_json(&ws, t, &policy)?),
        None => None,
    };
    let latest = match &open {
        Some(t) => ws.store.latest_checkpoint(&t.record.id)?,
        None => None,
    };
    human.push(format!("worktree context {}", ws.ctx.id));
    if let Some(old) = &ws.replaced_context {
        warnings.push(format!(
            "this directory carried context {old} from another path or machine; created a new context"
        ));
    }
    match (&open, &latest) {
        (Some(t), Some(l)) => human.push(format!(
            "open trace {} ({} checkpoints, latest {} at {}, writer {})",
            t.record.id,
            l.seq,
            l.id,
            fmt_time(l.created_at),
            t.writer.as_deref().unwrap_or("none")
        )),
        (Some(t), None) => human.push(format!("open trace {} (no checkpoints)", t.record.id)),
        _ => human.push("no open trace".into()),
    }

    let unsynced = ws.store.unsynced_count()?;
    let failures = ws.store.failures_since(now - DAY_MS)?;
    let last_sync_error: Option<Value> = ws
        .store
        .kv_get("last_sync_error")?
        .and_then(|s| serde_json::from_str(&s).ok());
    let last_sync_at: Option<i64> = ws
        .store
        .kv_get("last_sync_at")?
        .and_then(|s| s.parse().ok());

    let server = match &ws.cfg.server {
        None => {
            warnings.push("no server configured; checkpoints are local only".into());
            json!(null)
        }
        Some(s) => {
            let url = config::normalize_url(&s.url)?;
            let trusted = config::trusted(&url)?.is_some();
            let creds = config::token(&url)?.is_some();
            if !trusted {
                warnings.push(format!("server {url} is not trusted; run `git hf trust`"));
            }
            if !creds {
                warnings.push(format!(
                    "no credentials for {url}; set GHF_TOKEN or run `git hf login`"
                ));
            }
            human.push(format!("server {url} repository {}", s.repository));
            json!({"url": url, "repository": s.repository, "trusted": trusted, "credentials": creds})
        }
    };
    human.push(format!("{unsynced} checkpoint(s) not synced"));
    let rejected = ws.store.rejected_count()?;
    if rejected > 0 {
        warnings.push(format!(
            "{rejected} checkpoint(s) were refused by the server and exist only on this machine"
        ));
    }
    if let Some(e) = &last_sync_error {
        warnings.push(format!(
            "last sync failed: {} ({})",
            e["message"].as_str().unwrap_or(""),
            e["code"].as_str().unwrap_or("")
        ));
    }
    if !failures.is_empty() {
        warnings.push(format!(
            "{} capture failure(s) in the last 24h",
            failures.len()
        ));
    }
    for t in ws.store.traces()? {
        let el = retention::eligibility(&t.record, now, &policy);
        if el.warn
            && let Some(at) = el.eligible_at
        {
            warnings.push(format!(
                "trace {} is closed but not finalized; eligible for pruning at {}",
                t.record.id,
                fmt_time(at)
            ));
        }
    }
    let mode = match ws.cfg.capture_mode()? {
        config::CaptureMode::Strict => "strict",
        config::CaptureMode::BestEffort => "best-effort",
    };
    for w in &warnings {
        human.push(format!("warning: {w}"));
    }
    Ok(Out::new(
        json!({
            "repository_root": ws.repo.root,
            "context": ws.ctx.id,
            "capture_mode": mode,
            "trace": trace,
            "latest_checkpoint": latest,
            "unsynced": unsynced,
            "rejected": rejected,
            "last_sync_at": last_sync_at,
            "last_sync_error": last_sync_error,
            "capture_failures": failures.iter().map(|(at, code, msg, strict)| json!({
                "at": at, "code": code, "message": msg, "strict": strict
            })).collect::<Vec<_>>(),
            "server": server,
            "warnings": warnings,
        }),
        human.join("\n"),
    ))
}

// ---------------------------------------------------------------------------
// checkpoint, sync

pub struct CheckpointArgs {
    pub message: Option<String>,
    pub annotations: Vec<String>,
    pub takeover: bool,
    pub trace_name: Option<String>,
    pub force: bool,
}

pub fn checkpoint(env: &Env, a: CheckpointArgs) -> Result<Out> {
    let req = Request {
        message: a.message,
        annotations: capture::parse_annotations(&a.annotations)?,
        writer: workspace::default_writer(),
        takeover: a.takeover,
        trace_name: a
            .trace_name
            .or_else(|| std::env::var("GHF_TRACE_NAME").ok()),
        force: a.force,
        ..Default::default()
    };
    let repo = Repo::discover(&env.cwd)?;
    let ws = match Workspace::open(&env.cwd) {
        Ok(ws) => ws,
        Err(f) => {
            capture::note_failure(None, Some(&repo.git_dir), &f, false);
            return Err(f);
        }
    };
    match capture::checkpoint(&ws, &req) {
        Ok(o) => {
            if o.created {
                remote::spawn_background(&ws);
            }
            Ok(checkpoint_out(&o))
        }
        Err(f) => {
            let strict = ws.cfg.capture_mode().ok() == Some(config::CaptureMode::Strict);
            capture::note_failure(Some(&ws), Some(&ws.repo.git_dir), &f, strict);
            Err(f)
        }
    }
}

fn checkpoint_out(o: &capture::Outcome) -> Out {
    let c = &o.checkpoint;
    let human = if o.created {
        format!(
            "checkpoint {} (trace {}, #{}, {} change(s))",
            c.id,
            c.trace_id,
            c.seq,
            c.changes.len()
        )
    } else {
        format!("unchanged; latest checkpoint is {}", c.id)
    };
    Out::new(
        json!({
            "created": o.created,
            "trace_created": o.trace_created,
            "checkpoint": c,
            "trace": o.trace,
        }),
        human,
    )
}

pub fn sync(env: &Env, background: bool) -> Result<Out> {
    let ws = env.ws()?;
    if background {
        let r = remote::background(&ws)?;
        return Ok(Out::new(json!({"report": r}), ""));
    }
    let Some(remote) = Remote::connect(&ws, env.prompt())? else {
        fail!(Config, "no server configured in .ghf");
    };
    let r = match remote::sync(&ws, &remote) {
        Ok(r) => r,
        Err(f) => {
            remote::record_sync_error(&ws, &f);
            return Err(f.with_data(json!({"pending": ws.store.unsynced_count()?})));
        }
    };
    let human = format!(
        "pushed {} checkpoint(s), {} object(s) ({} bytes); pulled {} trace(s), {} checkpoint(s); {} pending",
        r.pushed_checkpoints,
        r.pushed_objects,
        r.pushed_bytes,
        r.pulled_traces,
        r.pulled_checkpoints,
        r.pending
    );
    Ok(Out::new(serde_json::to_value(&r)?, human))
}

/// Resolve a checkpoint, pulling from the server once if it is unknown.
fn resolve_checkpoint(ws: &Workspace, r: &str, prompt: bool) -> Result<CheckpointRecord> {
    match refs::checkpoint(ws, r) {
        Err(f) if f.code == ghf_proto::ErrorCode::NotFound && !r.starts_with('@') => {
            if let Some(remote) = Remote::connect(ws, prompt)? {
                remote::pull(ws, &remote, &mut remote::SyncReport::default())?;
                return refs::checkpoint(ws, r);
            }
            Err(f)
        }
        other => other,
    }
}

fn resolve_trace(ws: &Workspace, r: Option<&str>, prompt: bool) -> Result<TraceRow> {
    match refs::trace(ws, r) {
        Err(f) if f.code == ghf_proto::ErrorCode::NotFound && r.is_some() => {
            if let Some(remote) = Remote::connect(ws, prompt)? {
                remote::pull(ws, &remote, &mut remote::SyncReport::default())?;
                return refs::trace(ws, r);
            }
            Err(f)
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// trace list / log / diff

pub fn trace_list(env: &Env, state: Option<String>, all: bool) -> Result<Out> {
    let ws = env.ws()?;
    let policy = policy(&ws)?;
    let want = match state.as_deref() {
        None => None,
        Some(s) => Some(TraceState::parse(s).ok_or_else(|| err!(Usage, "unknown state {s:?}"))?),
    };
    let mut items = Vec::new();
    let mut human = Vec::new();
    for t in ws.store.traces()? {
        if want.is_some_and(|w| w != t.record.state)
            || (!all && want.is_none() && t.record.state == TraceState::Pruned)
        {
            continue;
        }
        let v = trace_json(&ws, &t, &policy)?;
        human.push(format!(
            "{}  {:<9} {:>4} ckpt  {}{}{}",
            t.record.id,
            t.record.state.as_str(),
            v["checkpoints"],
            fmt_time(t.record.created_at),
            t.record
                .name
                .as_deref()
                .map(|n| format!("  {n}"))
                .unwrap_or_default(),
            if t.record.context_id == ws.ctx.id {
                "  (this worktree)"
            } else {
                ""
            }
        ));
        items.push(v);
    }
    if human.is_empty() {
        human.push("no traces".into());
    }
    Ok(Out::new(json!({"traces": items}), human.join("\n")))
}

pub fn trace_log(
    env: &Env,
    trace: Option<String>,
    filters: Vec<String>,
    limit: Option<usize>,
) -> Result<Out> {
    let ws = env.ws()?;
    let t = resolve_trace(&ws, trace.as_deref(), env.prompt())?;
    let filters = capture::parse_annotations(&filters)?;
    let mut cks: Vec<CheckpointRecord> = ws
        .store
        .checkpoints(&t.record.id)?
        .into_iter()
        .filter(|c| filters.iter().all(|(k, v)| c.annotations.get(k) == Some(v)))
        .collect();
    if let Some(n) = limit {
        let skip = cks.len().saturating_sub(n);
        cks.drain(..skip);
    }
    let mut human = vec![format!(
        "trace {} ({}){}",
        t.record.id,
        t.record.state.as_str(),
        t.record
            .forked_from
            .as_deref()
            .map(|f| format!(", forked from {f}"))
            .unwrap_or_default()
    )];
    for c in cks.iter().rev() {
        let ann: Vec<String> = c
            .annotations
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        human.push(format!(
            "#{:<4} {}  {}  {:>3} change(s)  {}{}{}",
            c.seq,
            c.id,
            fmt_time(c.created_at),
            c.changes.len(),
            c.author,
            c.message
                .as_deref()
                .map(|m| format!("  {m}"))
                .unwrap_or_default(),
            if ann.is_empty() {
                String::new()
            } else {
                format!("  [{}]", ann.join(" "))
            }
        ));
    }
    Ok(Out::new(
        json!({"trace": t.record, "checkpoints": cks}),
        human.join("\n"),
    ))
}

/// One side of a diff.
enum Side {
    Checkpoint(Box<CheckpointRecord>),
    Worktree(Manifest),
}

fn read_side(ws: &Workspace, side: &Side, path: &str, e: &FileEntry) -> Result<Vec<u8>> {
    match side {
        Side::Checkpoint(_) => ws.store.blob(&e.id),
        Side::Worktree(_) => {
            let full = ws.repo.root.join(path);
            if e.mode == Mode::Symlink {
                Ok(crate::platform::read_link_bytes(&full)?)
            } else {
                Ok(fs::read(full)?)
            }
        }
    }
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0) || std::str::from_utf8(b).is_err()
}

pub fn trace_diff(env: &Env, a: String, b: Option<String>, patch: bool) -> Result<Out> {
    let ws = env.ws()?;
    let prompt = env.prompt();
    let ca = resolve_checkpoint(&ws, &a, prompt)?;
    remote::ensure_objects(&ws, &ca.root, prompt)?;
    let side_b = match &b {
        Some(b) => {
            let cb = resolve_checkpoint(&ws, b, prompt)?;
            remote::ensure_objects(&ws, &cb.root, prompt)?;
            Side::Checkpoint(Box::new(cb))
        }
        None => {
            let opts = capture::scan_options(&ws, false)?;
            Side::Worktree(scan::scan(&ws.repo, &ws.store, &opts)?.files)
        }
    };
    let deltas = match &side_b {
        Side::Checkpoint(cb) => scan::diff_trees(&ws.store, Some(&ca.root), Some(&cb.root))?,
        Side::Worktree(m) => scan::diff_manifests(&scan::flatten(&ws.store, &ca.root)?, m),
    };
    let changes = scan::changes(&deltas);
    let side_a = Side::Checkpoint(Box::new(ca.clone()));
    let mut text = String::new();
    if patch || !env.json {
        for d in &deltas {
            let old = match &d.old {
                Some(e) => Some(read_side(&ws, &side_a, &d.path, e)?),
                None => None,
            };
            let new = match &d.new {
                Some(e) => Some(read_side(&ws, &side_b, &d.path, e)?),
                None => None,
            };
            let a_name = if old.is_some() {
                format!("a/{}", d.path)
            } else {
                "/dev/null".into()
            };
            let b_name = if new.is_some() {
                format!("b/{}", d.path)
            } else {
                "/dev/null".into()
            };
            text.push_str(&format!("diff --ghf a/{0} b/{0}\n", d.path));
            if let (Some(o), Some(n)) = (&d.old, &d.new)
                && o.mode != n.mode
            {
                text.push_str(&format!("mode {:?} -> {:?}\n", o.mode, n.mode));
            }
            let (ob, nb) = (old.unwrap_or_default(), new.unwrap_or_default());
            if is_binary(&ob) || is_binary(&nb) {
                if ob != nb {
                    text.push_str(&format!("Binary files {a_name} and {b_name} differ\n"));
                }
                continue;
            }
            let (os, ns) = (String::from_utf8_lossy(&ob), String::from_utf8_lossy(&nb));
            let diff = similar::TextDiff::from_lines(os.as_ref(), ns.as_ref());
            text.push_str(
                &diff
                    .unified_diff()
                    .context_radius(3)
                    .header(&a_name, &b_name)
                    .to_string(),
            );
        }
    }
    let to = match &side_b {
        Side::Checkpoint(c) => json!(c.id),
        Side::Worktree(_) => json!("worktree"),
    };
    let mut data = json!({"from": ca.id, "to": to, "changes": changes});
    if patch {
        data["patch"] = json!(text);
    }
    let human = if text.is_empty() {
        "no differences".to_owned()
    } else {
        text.trim_end().to_owned()
    };
    Ok(Out::new(data, human))
}

// ---------------------------------------------------------------------------
// restore / fork / apply

fn default_worktree_path(ws: &Workspace, ck: &CheckpointRecord) -> PathBuf {
    let root = &ws.repo.root;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let tail = &ck.id[ck.id.len() - 8..];
    let parent = root.parent().unwrap_or(Path::new("/"));
    // Restoring the same checkpoint again gets the next free name.
    (1..)
        .map(|n| match n {
            1 => parent.join(format!("{name}-ghf-{tail}")),
            n => parent.join(format!("{name}-ghf-{tail}-{n}")),
        })
        .find(|p| !p.exists())
        .expect("a free name exists")
}

/// Restage a checkpoint's captured index: reset the index to its base
/// revision, then stage the recorded content, as `git add` would. Returns
/// the number of staged paths.
fn restage(repo: &Repo, store: &crate::store::Store, ck: &CheckpointRecord) -> Result<usize> {
    let Some(ix) = &ck.index else { return Ok(0) };
    let rev = ck
        .base_rev
        .as_deref()
        .and_then(|r| repo.resolve_commit(r))
        .or_else(|| repo.head())
        .ok_or_else(|| err!(Usage, "no base revision to restage against"))?;
    repo.read_tree(&rev)?;
    let mut entries = Vec::new();
    for (path, e) in scan::flatten(store, &ix.staged)? {
        entries.push((path, e.mode.git_mode().to_owned(), store.blob(&e.id)?));
    }
    repo.stage(&entries, &ix.removed, &store.tmp_dir())?;
    Ok(entries.len() + ix.removed.len())
}

/// Restore `ck` into a new Git worktree with a new context and a new trace
/// forked from `ck`. Shared by `restore` and `fork`.
fn restore_new_worktree(
    env: &Env,
    ws: &Workspace,
    ck: &CheckpointRecord,
    path: Option<PathBuf>,
    index: bool,
) -> Result<Out> {
    let path = match path {
        Some(p) if p.is_absolute() => p,
        Some(p) => env.cwd.join(p),
        None => default_worktree_path(ws, ck),
    };
    if path.exists()
        && fs::read_dir(&path)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        fail!(PathExists, "{} already exists", path.display());
    }
    for root in if index { ck.roots() } else { vec![ck.root] } {
        remote::ensure_objects(ws, &root, env.prompt())?;
    }
    let mut warnings = Vec::new();
    let rev = match ck
        .base_rev
        .as_deref()
        .and_then(|r| ws.repo.resolve_commit(r))
    {
        Some(r) => r,
        None => {
            let head = ws.repo.head().ok_or_else(|| {
                err!(
                    Usage,
                    "the repository has no commits; cannot create a worktree"
                )
            })?;
            if let Some(b) = &ck.base_rev {
                warnings.push(format!(
                    "base revision {b} is not available locally (fetch it to match exactly); the new worktree is based on HEAD {head}, with the checkpoint's files restored"
                ));
            }
            head
        }
    };
    ws.repo.worktree_add(&path, &rev)?;
    let path = crate::platform::canonicalize(&path).unwrap_or(path);
    let new_repo = Repo::discover(&path)?;
    let new_ws = Workspace::with_config(new_repo, ws.cfg.clone())?;
    let target = scan::flatten(&ws.store, &ck.root)?;
    let current = scan::scan(
        &new_ws.repo,
        &new_ws.store,
        &capture::scan_options(&new_ws, false)?,
    )?;
    let stats =
        materialize::materialize(&new_ws.repo.root, &new_ws.store, &target, &current.files)?;
    warnings.extend(stats.warnings.iter().cloned());
    let restaged = if index {
        restage(&new_ws.repo, &new_ws.store, ck)?
    } else {
        if ck.index.is_some() {
            warnings.push("the checkpoint has staged changes; pass --index to restage them".into());
        }
        0
    };
    let _lock = capture::lock(&new_ws)?;
    // Content, verification and the new trace commit together.
    let o = new_ws.store.tx(|| {
        let snap = scan::scan(
            &new_ws.repo,
            &new_ws.store,
            &capture::scan_options(&new_ws, true)?,
        )?;
        // Submodules may legitimately sit at another commit (warned above).
        if scan::without_gitlinks(&snap.files) != scan::without_gitlinks(&target) {
            fail!(
                Internal,
                "restored worktree does not match checkpoint {} (got {})",
                ck.id,
                snap.root.short()
            );
        }
        let req = Request {
            message: Some(format!("restored from {}", ck.id)),
            annotations: BTreeMap::from([("restored_from".into(), ck.id.clone())]),
            writer: workspace::default_writer(),
            force: true,
            ..Default::default()
        };
        let o = capture::record(&new_ws, None, snap, &req, Some(ck))?;
        // The new trace is unclaimed until its first writer checkpoints.
        new_ws.store.set_writer(&o.trace.id, None, 0)?;
        Ok(o)
    })?;
    remote::spawn_background(&new_ws);
    let mut human = format!(
        "restored {} into {}\nnew trace {} (context {})",
        ck.id,
        path.display(),
        o.trace.id,
        new_ws.ctx.id
    );
    for w in &warnings {
        human.push_str(&format!("\nwarning: {w}"));
    }
    Ok(Out::new(
        json!({
            "worktree": path,
            "base_rev": rev,
            "context": new_ws.ctx.id,
            "trace": o.trace,
            "checkpoint": o.checkpoint,
            "restored_from": ck.id,
            "files_written": stats.written,
            "restaged": restaged,
            "warnings": warnings,
        }),
        human,
    ))
}

pub fn trace_restore(
    env: &Env,
    r: String,
    worktree: Option<PathBuf>,
    in_place: bool,
    force: bool,
    index: bool,
) -> Result<Out> {
    let ws = env.ws()?;
    let ck = resolve_checkpoint(&ws, &r, env.prompt())?;
    if !in_place {
        return restore_new_worktree(env, &ws, &ck, worktree, index);
    }
    for root in if index { ck.roots() } else { vec![ck.root] } {
        remote::ensure_objects(&ws, &root, env.prompt())?;
    }
    let writer = workspace::default_writer();
    let _lock = capture::lock(&ws)?;
    let open = ws.store.open_trace(&ws.ctx.id)?;
    if let Some(t) = &open {
        capture::check_writer(&ws, t, &writer, false)?;
    }
    // Scan once: the guard, the pre-restore checkpoint and the restore all
    // see the same state. The checkpoint is durable before the tree changes.
    let (current, pre) = ws.store.tx(|| {
        let snap = scan::scan(&ws.repo, &ws.store, &capture::scan_options(&ws, true)?)?;
        let latest = match &open {
            Some(t) => ws.store.latest_checkpoint(&t.record.id)?,
            None => None,
        };
        // With --index the index is rewritten too, so it must be captured.
        let index_clean = !index || latest.as_ref().map(|l| &l.index) == Some(&capture::index_state(&ws)?);
        if index_clean && latest.as_ref().is_some_and(|l| l.root == snap.root) {
            return Ok((snap, None));
        }
        if !force {
            return Err(err!(
                WorktreeDirty,
                "the working tree has changes not captured in a checkpoint; run `git hf checkpoint` first, restore into a new worktree, or pass --force (which checkpoints first)"
            )
            .with_data(json!({
                "latest_checkpoint": latest.as_ref().map(|l| &l.id),
            })));
        }
        let req = Request {
            message: Some(format!("before forced restore of {}", ck.id)),
            writer: writer.clone(),
            ..Default::default()
        };
        let o = capture::record(&ws, open.clone(), snap.clone(), &req, None)?;
        Ok((snap, Some(o.checkpoint)))
    })?;
    let target = scan::flatten(&ws.store, &ck.root)?;
    let stats = materialize::materialize(&ws.repo.root, &ws.store, &target, &current.files)?;
    let mut warnings = stats.warnings.clone();
    let restaged = if index {
        restage(&ws.repo, &ws.store, &ck)?
    } else {
        if ck.index.is_some() {
            warnings.push("the checkpoint has staged changes; pass --index to restage them".into());
        }
        0
    };
    let req = Request {
        message: Some(format!("restored {}", ck.id)),
        annotations: BTreeMap::from([("restored_from".into(), ck.id.clone())]),
        writer,
        force: true,
        ..Default::default()
    };
    let o = capture::checkpoint_locked(&ws, &req)?;
    let got = scan::flatten(&ws.store, &o.checkpoint.root)?;
    if scan::without_gitlinks(&got) != scan::without_gitlinks(&target) {
        let undo = pre
            .as_ref()
            .map(|p| p.id.clone())
            .or_else(|| o.checkpoint.parent.clone())
            .unwrap_or_else(|| "@~1".into());
        fail!(
            Internal,
            "worktree does not match {} after restore (something wrote to it concurrently?); recover with `git hf trace restore {undo} --in-place --force`",
            ck.id
        );
    }
    remote::spawn_background(&ws);
    Ok(Out::new(
        json!({
            "restored_from": ck.id,
            "checkpoint": o.checkpoint,
            "pre_restore_checkpoint": pre.as_ref().map(|p| &p.id),
            "files_written": stats.written,
            "files_deleted": stats.deleted,
            "restaged": restaged,
            "warnings": warnings,
        }),
        format!(
            "restored {} in place ({} written, {} deleted); recorded as {}",
            ck.id, stats.written, stats.deleted, o.checkpoint.id
        ),
    ))
}

pub fn trace_fork(env: &Env, r: String, worktree: Option<PathBuf>, index: bool) -> Result<Out> {
    let ws = env.ws()?;
    let ck = resolve_checkpoint(&ws, &r, env.prompt())?;
    restore_new_worktree(env, &ws, &ck, worktree, index)
}

#[derive(Debug, serde::Serialize)]
struct Conflict {
    path: String,
    reason: &'static str,
    /// Cannot be written as conflict markers; always refuses the apply.
    #[serde(skip)]
    hard: bool,
}

/// Merge `a..b` onto `current`: the target manifest and any conflicts.
/// Merged blobs are stored in the local store.
fn plan_apply(
    ws: &Workspace,
    deltas: &[scan::Delta],
    current: &Manifest,
    write_conflicts: bool,
) -> Result<(Manifest, Vec<Conflict>)> {
    let mut target = current.clone();
    let mut conflicts = Vec::new();
    let tmp = ws.store.tmp_dir();
    let soft = |path: &str, reason| Conflict {
        path: path.to_owned(),
        reason,
        hard: false,
    };

    for d in deltas {
        let ours = current.get(&d.path).copied();
        if ours == d.new {
            continue;
        }
        if ours == d.old {
            match d.new {
                Some(n) => target.insert(d.path.clone(), n),
                None => target.remove(&d.path),
            };
            continue;
        }
        let regular = |e: &Option<FileEntry>| {
            e.is_none_or(|e| e.mode != Mode::Symlink && e.mode != Mode::Gitlink)
        };
        let (Some(o), Some(n)) = (ours, d.new) else {
            conflicts.push(soft(
                &d.path,
                if d.new.is_none() {
                    "deleted incoming, modified in working tree"
                } else {
                    "modified incoming, deleted in working tree"
                },
            ));
            continue;
        };
        if !regular(&Some(o)) || !regular(&Some(n)) || !regular(&d.old) {
            conflicts.push(soft(&d.path, "symlink or submodule changed on both sides"));
            continue;
        }
        let base = match d.old {
            Some(e) => ws.store.blob(&e.id)?,
            None => Vec::new(),
        };
        let (ob, nb) = (ws.store.blob(&o.id)?, ws.store.blob(&n.id)?);
        if is_binary(&ob) || is_binary(&nb) || is_binary(&base) {
            conflicts.push(soft(&d.path, "binary file changed on both sides"));
            continue;
        }
        let tag = ghf_proto::id::random_hex(6);
        let (po, pb, pt) = (
            tmp.join(format!("ours-{tag}")),
            tmp.join(format!("base-{tag}")),
            tmp.join(format!("theirs-{tag}")),
        );
        fs::write(&po, &ob)?;
        fs::write(&pb, &base)?;
        fs::write(&pt, &nb)?;
        let merged = ws.repo.merge_file(&po, &pb, &pt);
        for p in [&po, &pb, &pt] {
            let _ = fs::remove_file(p);
        }
        let (bytes, conflicted) = merged?;
        if conflicted {
            conflicts.push(soft(&d.path, "content changed on both sides"));
            if !write_conflicts {
                continue;
            }
        }
        let id = ws.store.put(ghf_proto::object::Kind::Blob, &bytes)?;
        let mode = if d.old.is_some_and(|b| b.mode == o.mode) {
            n.mode
        } else {
            o.mode
        };
        target.insert(
            d.path.clone(),
            FileEntry {
                mode,
                id,
                size: bytes.len() as u64,
            },
        );
    }
    // File/directory collisions and uncaptured files in the way.
    for path in materialize::blockers(&ws.repo.root, &target, current) {
        conflicts.push(Conflict {
            path,
            reason: "file/directory collision or uncaptured file in the way",
            hard: true,
        });
    }
    Ok((target, conflicts))
}

pub fn trace_apply(env: &Env, range: String, write_conflicts: bool) -> Result<Out> {
    let ws = env.ws()?;
    let prompt = env.prompt();
    let (a, b) = match range.split_once("..") {
        Some((a, "")) => {
            let ca = resolve_checkpoint(&ws, a, prompt)?;
            let cb = ws
                .store
                .latest_checkpoint(&ca.trace_id)?
                .ok_or_else(|| err!(NotFound, "trace {} is empty", ca.trace_id))?;
            (ca, cb)
        }
        Some((a, b)) => (
            resolve_checkpoint(&ws, a, prompt)?,
            resolve_checkpoint(&ws, b, prompt)?,
        ),
        None => {
            let cb = resolve_checkpoint(&ws, &range, prompt)?;
            let parent = cb
                .parent
                .as_deref()
                .ok_or_else(|| err!(Usage, "{} has no parent; use A..B", cb.id))?;
            (resolve_checkpoint(&ws, parent, prompt)?, cb)
        }
    };
    remote::ensure_objects(&ws, &a.root, prompt)?;
    remote::ensure_objects(&ws, &b.root, prompt)?;
    let writer = workspace::default_writer();
    let _lock = capture::lock(&ws)?;
    let open = ws.store.open_trace(&ws.ctx.id)?;
    if let Some(t) = &open {
        capture::check_writer(&ws, t, &writer, false)?;
    }
    let deltas = scan::diff_trees(&ws.store, Some(&a.root), Some(&b.root))?;
    // Scan, merge and the pre-apply checkpoint commit together, before the
    // tree changes. A refusal rolls everything back.
    let (current, target, conflicts, pre) = ws.store.tx(|| {
        let current = scan::scan(&ws.repo, &ws.store, &capture::scan_options(&ws, true)?)?;
        let (target, conflicts) = plan_apply(&ws, &deltas, &current.files, write_conflicts)?;
        if !conflicts.is_empty() && (!write_conflicts || conflicts.iter().any(|c| c.hard)) {
            return Err(err!(
                ApplyConflict,
                "{} conflicting path(s); the working tree is unchanged",
                conflicts.len()
            )
            .with_data(
                json!({"applied": false, "from": a.id, "to": b.id, "conflicts": conflicts}),
            ));
        }
        let latest = match &open {
            Some(t) => ws.store.latest_checkpoint(&t.record.id)?,
            None => None,
        };
        let mut pre = None;
        if latest.as_ref().is_none_or(|l| l.root != current.root) {
            let req = Request {
                message: Some(format!("before apply {}..{}", a.id, b.id)),
                writer: writer.clone(),
                ..Default::default()
            };
            pre = Some(
                capture::record(&ws, open.clone(), current.clone(), &req, None)?
                    .checkpoint
                    .id,
            );
        }
        Ok((current, target, conflicts, pre))
    })?;
    let stats = materialize::materialize(&ws.repo.root, &ws.store, &target, &current.files)?;
    let req = Request {
        message: Some(format!("applied {}..{}", a.id, b.id)),
        annotations: BTreeMap::from([
            ("applied_from".into(), a.id.clone()),
            ("applied_to".into(), b.id.clone()),
        ]),
        writer,
        force: true,
        ..Default::default()
    };
    let o = capture::checkpoint_locked(&ws, &req)?;
    let got = scan::flatten(&ws.store, &o.checkpoint.root)?;
    if scan::without_gitlinks(&got) != scan::without_gitlinks(&target) {
        let undo = pre
            .clone()
            .or_else(|| o.checkpoint.parent.clone())
            .unwrap_or_default();
        fail!(
            Internal,
            "worktree does not match the planned apply result (something wrote to it concurrently?); undo with `git hf trace restore {undo} --in-place --force`"
        );
    }
    remote::spawn_background(&ws);
    let data = json!({
        "applied": true,
        "from": a.id,
        "to": b.id,
        "checkpoint": o.checkpoint,
        "pre_apply_checkpoint": pre,
        "files_written": stats.written,
        "files_deleted": stats.deleted,
        "conflicts": conflicts,
    });
    if !conflicts.is_empty() {
        return Err(err!(
            ApplyConflict,
            "applied with {} conflict(s) written as markers; resolve them and checkpoint",
            conflicts.len()
        )
        .with_data(data));
    }
    Ok(Out::new(
        data,
        format!(
            "applied {}..{} ({} written, {} deleted); recorded as {}",
            a.id, b.id, stats.written, stats.deleted, o.checkpoint.id
        ),
    ))
}

// ---------------------------------------------------------------------------
// lifecycle

fn save_trace(ws: &Workspace, t: &TraceRecord) -> Result<()> {
    ws.store.update_trace(t, true)?;
    remote::spawn_background(ws);
    Ok(())
}

pub fn trace_close(env: &Env, trace: Option<String>) -> Result<Out> {
    let ws = env.ws()?;
    let row = resolve_trace(&ws, trace.as_deref(), env.prompt())?;
    let mut t = row.record;
    let was = t.state;
    if was == TraceState::Open {
        t.state = TraceState::Closed;
        t.closed_at = Some(ghf_proto::id::now_ms());
        save_trace(&ws, &t)?;
        ws.store.set_writer(&t.id, None, 0)?;
    }
    let human = if was == TraceState::Open {
        format!("closed trace {}", t.id)
    } else {
        format!("trace {} is already {}", t.id, was.as_str())
    };
    Ok(Out::new(
        json!({"trace": t, "changed": was == TraceState::Open}),
        human,
    ))
}

fn main_ref(ws: &Workspace) -> Result<String> {
    if let Some(r) = ws.cfg.main_ref() {
        return Ok(r.to_owned());
    }
    for r in [
        "origin/HEAD",
        "origin/main",
        "origin/master",
        "main",
        "master",
    ] {
        if ws.repo.resolve_commit(r).is_some() {
            return Ok(r.to_owned());
        }
    }
    fail!(
        Config,
        "cannot find the main branch; set [git] main in .ghf"
    )
}

pub fn trace_finalize(
    env: &Env,
    trace: Option<String>,
    commit: Option<String>,
    abandoned: bool,
) -> Result<Out> {
    let ws = env.ws()?;
    let row = resolve_trace(&ws, trace.as_deref(), env.prompt())?;
    let mut t = row.record;
    let sha = match &commit {
        Some(c) => {
            let sha = ws.repo.resolve_commit(c).ok_or_else(|| {
                err!(
                    CommitNotFound,
                    "commit {c} does not exist locally; fetch it first"
                )
            })?;
            let main = main_ref(&ws)?;
            if !ws.repo.is_ancestor(&sha, &main) {
                fail!(
                    CommitNotReachable,
                    "commit {sha} is not reachable from {main}"
                );
            }
            Some(sha)
        }
        None => None,
    };
    match t.state {
        TraceState::Finalized | TraceState::Pruned => {
            if t.final_commit == sha && t.abandoned == abandoned {
                return Ok(Out::new(
                    json!({"trace": t, "changed": false}),
                    format!("trace {} is already finalized", t.id),
                ));
            }
            fail!(TraceState, "trace {} is already {}", t.id, t.state.as_str());
        }
        TraceState::Open | TraceState::Closed => {}
    }
    let now = ghf_proto::id::now_ms();
    if t.state == TraceState::Open {
        t.closed_at = Some(now);
        ws.store.set_writer(&t.id, None, 0)?;
    }
    t.state = TraceState::Finalized;
    t.finalized_at = Some(now);
    t.final_commit = sha.clone();
    t.abandoned = abandoned;
    save_trace(&ws, &t)?;
    let human = match &sha {
        Some(s) => format!("finalized trace {} against {s}", t.id),
        None => format!("finalized trace {} as abandoned", t.id),
    };
    Ok(Out::new(json!({"trace": t, "changed": true}), human))
}

pub fn trace_hold(
    env: &Env,
    trace: String,
    name: String,
    expires: Option<String>,
    release: bool,
) -> Result<Out> {
    let ws = env.ws()?;
    let row = resolve_trace(&ws, Some(&trace), env.prompt())?;
    let mut t = row.record;
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        fail!(
            Usage,
            "hold names use letters, digits, '.', '_' and '-' (at most 64)"
        );
    }
    let now = ghf_proto::id::now_ms();
    let expires_at = match expires {
        Some(s) => Some(
            now + retention::parse_duration_ms(&s)
                .ok_or_else(|| err!(Usage, "invalid duration {s:?}; use e.g. 7d"))?,
        ),
        None => None,
    };
    t.holds.retain(|h| h.name != name);
    if !release {
        t.holds.push(Hold {
            name: name.clone(),
            expires_at,
            created_at: now,
        });
    }
    ws.store.update_trace(&t, false)?;
    ws.store.add_hold_op(&t.id, &name, !release, expires_at)?;
    remote::spawn_background(&ws);
    let human = if release {
        format!("released hold {name:?} on {}", t.id)
    } else {
        format!("placed hold {name:?} on {}", t.id)
    };
    Ok(Out::new(json!({"trace": t}), human))
}

// ---------------------------------------------------------------------------
// prune

pub fn prune(env: &Env, dry_run: bool) -> Result<Out> {
    let ws = env.ws()?;
    let mut server_report = None;
    if let Some(remote) = Remote::connect(&ws, env.prompt())? {
        // Push first so the server knows about everything this client holds.
        remote::sync(&ws, &remote)?;
        server_report = Some(remote.prune(dry_run)?);
        if !dry_run {
            remote::pull(&ws, &remote, &mut remote::SyncReport::default())?;
        }
    }
    let local = prune_local(&ws, dry_run, server_report.is_some())?;
    let human = format!(
        "{}server: {}\nlocal: pruned {} trace(s), deleted {} object(s)",
        if dry_run { "dry run\n" } else { "" },
        match &server_report {
            Some(r) => format!(
                "pruned {} trace(s), deleted {} object(s)",
                r.pruned.len(),
                r.objects_deleted
            ),
            None => "none configured".into(),
        },
        local["pruned"].as_array().map_or(0, Vec::len),
        local["objects_deleted"]
    );
    Ok(Out::new(
        json!({"dry_run": dry_run, "server": server_report, "local": local}),
        human,
    ))
}

/// Drop local payloads of pruned traces (or, with no server, of eligible
/// fully-local traces), keeping fork bases, then collect unreferenced objects.
fn prune_local(ws: &Workspace, dry_run: bool, has_server: bool) -> Result<Value> {
    let now = ghf_proto::id::now_ms();
    let policy = policy(ws)?;
    let traces = ws.store.traces()?;
    let mut doomed = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for t in &traces {
        let prunable = if has_server {
            // The server decides; never drop anything not yet uploaded.
            t.record.state == TraceState::Pruned && ws.store.unsynced_in_trace(&t.record.id)? == 0
        } else {
            t.record.state == TraceState::Pruned
                || retention::eligibility(&t.record, now, &policy).eligible
        };
        if prunable {
            doomed.push(t.record.clone());
        } else if let Some(f) = &t.record.forked_from {
            // A retained fork keeps its base checkpoint restorable.
            keep.push(f.clone());
        }
    }
    let mut pruned = Vec::new();
    for t in &doomed {
        let doomed_rows = ws
            .store
            .checkpoints(&t.id)?
            .iter()
            .filter(|c| !keep.contains(&c.id))
            .count();
        if doomed_rows == 0 && t.state == TraceState::Pruned {
            continue;
        }
        pruned.push(t.id.clone());
        if dry_run {
            continue;
        }
        ws.store.delete_trace_rows(&t.id, &keep)?;
        if t.state != TraceState::Pruned {
            let mut r = t.clone();
            r.state = TraceState::Pruned;
            r.pruned_at = Some(now);
            ws.store.update_trace(&r, false)?;
        }
    }
    let (objects_deleted, bytes) = if dry_run { (0, 0) } else { gc(ws)? };
    Ok(
        json!({"pruned": pruned, "retained_checkpoints": keep, "objects_deleted": objects_deleted, "bytes_freed": bytes}),
    )
}

/// Delete objects no remaining checkpoint references. Objects younger than
/// the grace period (default one hour, `GHF_GC_GRACE`) are kept: a capture
/// may be writing them right now.
fn gc(ws: &Workspace) -> Result<(u64, u64)> {
    let grace = std::env::var("GHF_GC_GRACE")
        .ok()
        .and_then(|s| retention::parse_duration_ms(&s))
        .unwrap_or(3_600_000);
    // Mark and sweep in one write transaction, so no capture can reference an
    // object between the mark and its deletion.
    ws.store.tx(|| {
        let mut live = std::collections::HashSet::new();
        let mut stack: Vec<ghf_proto::ObjectId> = ws
            .store
            .all_checkpoints()?
            .iter()
            .flat_map(|c| c.roots())
            .collect();
        while let Some(id) = stack.pop() {
            if !live.insert(id) {
                continue;
            }
            if let Ok(entries) = ws.store.tree(&id) {
                for e in entries {
                    if e.mode == Mode::Tree {
                        stack.push(e.id);
                    } else {
                        live.insert(e.id);
                    }
                }
            }
        }
        ws.store.sweep(&live, grace)
    })
}
