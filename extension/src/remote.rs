//! Server client, trust checks and synchronization.

use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use ghf_proto::api::{
    ApiError, Changes, HoldRequest, ObjectList, PendingReport, PruneReport, PruneRequest,
    ServerInfo,
};
use ghf_proto::object::Mode;
use ghf_proto::{CheckpointRecord, ErrorCode, ObjectId, Policy, TraceRecord};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config;
use crate::error::{Failure, Result};
use crate::workspace::{FileLock, Workspace};
use crate::{err, fail};

pub struct Remote {
    agent: ureq::Agent,
    pub url: String,
    pub repo: String,
    token: Option<String>,
    verified: std::cell::Cell<bool>,
    expected_server_id: Option<String>,
}

/// Prompts are allowed only for a human at a terminal without `--json`.
pub fn interactive(json: bool) -> bool {
    !json && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Fetch `/v1/info` from a server without any trust or auth.
pub fn fetch_info(url: &str) -> Result<ServerInfo> {
    let mut resp = agent()
        .get(format!("{url}/v1/info"))
        .call()
        .map_err(|e| err!(Server, "cannot reach {url}: {e}"))?;
    if !resp.status().is_success() {
        fail!(Server, "{url} returned HTTP {}", resp.status().as_u16());
    }
    resp.body_mut()
        .read_json()
        .map_err(|e| err!(Server, "{url} is not a ghf server: {e}"))
}

impl Remote {
    /// Connect to the configured server, enforcing trust. Returns `None`
    /// when no server is configured.
    pub fn connect(ws: &Workspace, prompt: bool) -> Result<Option<Self>> {
        let Some(server) = &ws.cfg.server else {
            return Ok(None);
        };
        let url = config::normalize_url(&server.url)?;
        let trusted = match config::trusted(&url)? {
            Some(t) => t,
            None if prompt => {
                let info = fetch_info(&url)?;
                eprint!(
                    "ghf: trust server {url} (id {}) to receive source from {}? [y/N] ",
                    info.server_id, server.repository
                );
                std::io::stderr().flush().ok();
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    fail!(UntrustedServer, "server {url} was not trusted");
                }
                config::trust(&url, &info.server_id)?;
                config::trusted(&url)?.expect("just trusted")
            }
            None => fail!(
                UntrustedServer,
                "server {url} is not trusted on this machine; run `git hf trust`"
            ),
        };
        Ok(Some(Self {
            agent: agent(),
            token: config::token(&url)?,
            url,
            repo: server.repository.clone(),
            verified: std::cell::Cell::new(false),
            expected_server_id: Some(trusted.server_id),
        }))
    }

    fn verify(&self) -> Result<()> {
        if self.verified.get() {
            return Ok(());
        }
        let info = fetch_info(&self.url)?;
        if let Some(expected) = &self.expected_server_id
            && &info.server_id != expected
        {
            fail!(
                ServerMismatch,
                "server {} now reports id {} but {expected} was trusted; re-run `git hf trust` if this is expected",
                self.url,
                info.server_id
            );
        }
        self.verified.set(true);
        Ok(())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/v1/repos/{}/{path}", self.url, self.repo)
    }

    fn auth(&self) -> Result<String> {
        match &self.token {
            Some(t) => Ok(format!("Bearer {t}")),
            None => fail!(
                Auth,
                "no credentials for {}; set GHF_TOKEN or run `git hf login`",
                self.url
            ),
        }
    }

    fn check(
        &self,
        mut resp: ureq::http::Response<ureq::Body>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let body: Option<ApiError> = resp.body_mut().read_json().ok();
        Err(match (status, body) {
            (_, Some(e)) => Failure::new(e.code, e.message),
            (401 | 403, None) => err!(Auth, "server rejected credentials (HTTP {status})"),
            (404, None) => err!(NotFound, "not found on server (HTTP 404)"),
            (_, None) => err!(Server, "server returned HTTP {status}"),
        })
    }

    fn net(&self, e: ureq::Error) -> Failure {
        err!(Server, "cannot reach {}: {e}", self.url)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.verify()?;
        let resp = self
            .agent
            .get(self.endpoint(path))
            .header("Authorization", self.auth()?)
            .call()
            .map_err(|e| self.net(e))?;
        let mut resp = self.check(resp)?;
        resp.body_mut()
            .read_json()
            .map_err(|e| err!(Server, "bad response from server: {e}"))
    }

    fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: &B,
    ) -> Result<Option<T>> {
        self.verify()?;
        let url = self.endpoint(path);
        let auth = self.auth()?;
        let resp = match method {
            "PUT" => self
                .agent
                .put(url)
                .header("Authorization", auth)
                .send_json(body),
            "POST" => self
                .agent
                .post(url)
                .header("Authorization", auth)
                .send_json(body),
            "DELETE" => self.agent.delete(url).header("Authorization", auth).call(),
            _ => unreachable!(),
        }
        .map_err(|e| self.net(e))?;
        let mut resp = self.check(resp)?;
        if resp.status().as_u16() == 204 {
            return Ok(None);
        }
        resp.body_mut()
            .read_json()
            .map(Some)
            .map_err(|e| err!(Server, "bad response from server: {e}"))
    }

    pub fn policy(&self) -> Result<Policy> {
        self.get_json("policy")
    }

    pub fn missing(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(1000) {
            let r: Option<ObjectList> = self.send_json(
                "POST",
                "objects/missing",
                &ObjectList {
                    ids: chunk.to_vec(),
                },
            )?;
            out.extend(r.unwrap_or_default().ids);
        }
        Ok(out)
    }

    /// Upload framed objects in one request.
    pub fn put_batch(&self, body: Vec<u8>) -> Result<()> {
        self.verify()?;
        let resp = self
            .agent
            .post(self.endpoint("objects/batch"))
            .header("Authorization", self.auth()?)
            .header("Content-Type", "application/octet-stream")
            .send(&body[..])
            .map_err(|e| self.net(e))?;
        self.check(resp).map(|_| ())
    }

    /// Download objects; ids the server lacks are omitted.
    pub fn fetch_batch(&self, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Vec<u8>)>> {
        self.verify()?;
        let resp = self
            .agent
            .post(self.endpoint("objects/fetch"))
            .header("Authorization", self.auth()?)
            .send_json(&ObjectList { ids: ids.to_vec() })
            .map_err(|e| self.net(e))?;
        let mut resp = self.check(resp)?;
        let body = resp
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|e| err!(Server, "download objects: {e}"))?;
        let frames = ghf_proto::api::unframe(&body)
            .ok_or_else(|| err!(Server, "malformed object batch from server"))?;
        Ok(frames.into_iter().map(|(id, d)| (id, d.to_vec())).collect())
    }

    pub fn put_trace(&self, t: &TraceRecord) -> Result<TraceRecord> {
        self.send_json::<_, TraceRecord>("PUT", &format!("traces/{}", t.id), t)?
            .ok_or_else(|| err!(Server, "server returned no trace"))
    }

    pub fn put_checkpoint(&self, c: &CheckpointRecord) -> Result<()> {
        self.send_json::<_, serde_json::Value>("PUT", &format!("checkpoints/{}", c.id), c)
            .map(|_| ())
    }

    pub fn put_hold(&self, trace: &str, name: &str, expires_at: Option<i64>) -> Result<()> {
        self.send_json::<_, TraceRecord>(
            "PUT",
            &format!("traces/{trace}/holds/{name}"),
            &HoldRequest { expires_at },
        )
        .map(|_| ())
    }

    pub fn delete_hold(&self, trace: &str, name: &str) -> Result<()> {
        self.send_json::<_, TraceRecord>("DELETE", &format!("traces/{trace}/holds/{name}"), &())
            .map(|_| ())
    }

    pub fn changes(&self, since: i64) -> Result<Changes> {
        self.get_json(&format!("changes?since={since}"))
    }

    pub fn report_pending(&self, client: &str, traces: Vec<String>) -> Result<()> {
        self.send_json::<_, serde_json::Value>(
            "PUT",
            &format!("clients/{client}/pending"),
            &PendingReport { traces },
        )
        .map(|_| ())
    }

    pub fn prune(&self, dry_run: bool) -> Result<PruneReport> {
        self.send_json::<_, PruneReport>("POST", "prune", &PruneRequest { dry_run })?
            .ok_or_else(|| err!(Server, "server returned no prune report"))
    }
}

// ---------------------------------------------------------------------------
// Sync

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub pushed_traces: usize,
    pub pushed_checkpoints: usize,
    pub pushed_objects: usize,
    pub pushed_bytes: u64,
    pub hold_ops: usize,
    pub pulled_traces: usize,
    pub pulled_checkpoints: usize,
    pub pending: i64,
}

fn client_id(ws: &Workspace) -> Result<String> {
    if let Some(id) = ws.store.kv_get("client_id")? {
        return Ok(id);
    }
    let id = format!("cl_{}", ghf_proto::id::random_hex(12));
    ws.store.kv_set("client_id", &id)?;
    Ok(id)
}

fn pending_traces(ws: &Workspace) -> Result<Vec<String>> {
    let mut ids: Vec<String> = ws
        .store
        .unsynced()?
        .into_iter()
        .map(|c| c.trace_id)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Upper bound for one upload request body.
const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Objects in the closure of `root` not known to be on the server, children
/// before parents.
fn closure_order(ws: &Workspace, root: &ObjectId, trust_cache: bool) -> Result<Vec<ObjectId>> {
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        ws: &Workspace,
        id: &ObjectId,
        trust_cache: bool,
        seen: &mut HashSet<ObjectId>,
        order: &mut Vec<ObjectId>,
    ) -> Result<()> {
        if !seen.insert(*id) || (trust_cache && ws.store.remote_has(id)) {
            return Ok(());
        }
        for e in ws.store.tree(id)? {
            if e.mode == Mode::Tree {
                walk(ws, &e.id, trust_cache, seen, order)?;
            } else if !(trust_cache && ws.store.remote_has(&e.id)) && seen.insert(e.id) {
                order.push(e.id);
            }
        }
        order.push(*id);
        Ok(())
    }
    walk(ws, root, trust_cache, &mut seen, &mut order)?;
    Ok(order)
}

/// Upload whatever part of `root`'s closure the server lacks, in batches,
/// children before parents.
fn upload_closure(
    ws: &Workspace,
    remote: &Remote,
    root: &ObjectId,
    trust_cache: bool,
    report: &mut SyncReport,
) -> Result<Vec<ObjectId>> {
    let order = closure_order(ws, root, trust_cache)?;
    if order.is_empty() {
        return Ok(order);
    }
    let missing: HashSet<ObjectId> = remote.missing(&order)?.into_iter().collect();
    let mut body = Vec::new();
    for id in order.iter().filter(|id| missing.contains(id)) {
        let data = ws.store.get_compressed(id)?.ok_or_else(|| {
            err!(
                ObjectMissing,
                "object {} missing from local store",
                id.short()
            )
        })?;
        ghf_proto::api::frame(&mut body, id, &data);
        report.pushed_objects += 1;
        report.pushed_bytes += data.len() as u64;
        if body.len() >= BATCH_BYTES {
            remote.put_batch(std::mem::take(&mut body))?;
        }
    }
    if !body.is_empty() {
        remote.put_batch(body)?;
    }
    Ok(order)
}

/// Push everything pending, then pull remote metadata.
pub fn sync(ws: &Workspace, remote: &Remote) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let client = client_id(ws)?;
    remote.report_pending(&client, pending_traces(ws)?)?;

    for t in ws.store.dirty_traces()? {
        remote.put_trace(&t)?;
        ws.store.mark_trace_clean(&t)?;
        report.pushed_traces += 1;
    }

    for ck in ws.store.unsynced()? {
        let mut sent = upload_closure(ws, remote, &ck.root, true, &mut report)?;
        match remote.put_checkpoint(&ck) {
            Err(f) if f.code == ErrorCode::ObjectMissing => {
                // The server lost objects we thought it had (pruned); stop
                // trusting the cache and check the whole closure.
                ws.store.forget_remote()?;
                sent = upload_closure(ws, remote, &ck.root, false, &mut report)?;
                remote.put_checkpoint(&ck)?;
            }
            r => r?,
        }
        // The server verified the closure: remember it to skip next time.
        ws.store.tx(|| {
            ws.store.mark_remote(&sent)?;
            ws.store.mark_synced(&ck.id)
        })?;
        report.pushed_checkpoints += 1;
    }

    for (op, trace, name, place, expires) in ws.store.hold_ops()? {
        if place {
            remote.put_hold(&trace, &name, expires)?;
        } else {
            remote.delete_hold(&trace, &name)?;
        }
        ws.store.remove_hold_op(op)?;
        report.hold_ops += 1;
    }

    remote.report_pending(&client, pending_traces(ws)?)?;
    pull(ws, remote, &mut report)?;
    if let Ok(p) = remote.policy() {
        ws.store
            .kv_set("server_policy", &serde_json::to_string(&p)?)?;
    }
    report.pending = ws.store.unsynced_count()?;
    ws.store
        .kv_set("last_sync_at", &ghf_proto::id::now_ms().to_string())?;
    ws.store.kv_delete("last_sync_error")?;
    Ok(report)
}

/// Pull trace and checkpoint metadata. Content is fetched lazily.
pub fn pull(ws: &Workspace, remote: &Remote, report: &mut SyncReport) -> Result<()> {
    let mut since: i64 = ws
        .store
        .kv_get("pull_cursor")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    loop {
        let ch = remote.changes(since)?;
        ws.store.tx(|| {
            for t in &ch.traces {
                match ws.store.trace(&t.id)? {
                    None => {
                        ws.store.insert_trace(t, false)?;
                    }
                    Some(_) => {
                        // Local edits not yet pushed win; the next push merges them.
                        let dirty: i64 = ws.store.db.query_row(
                            "SELECT dirty FROM traces WHERE id = ?1",
                            [&t.id],
                            |r| r.get(0),
                        )?;
                        if dirty == 0 {
                            ws.store.update_trace(t, false)?;
                        }
                    }
                }
                report.pulled_traces += 1;
            }
            for c in &ch.checkpoints {
                ws.store.insert_checkpoint(c, true)?;
                report.pulled_checkpoints += 1;
            }
            ws.store.kv_set("pull_cursor", &ch.next.to_string())?;
            Ok(())
        })?;
        since = ch.next;
        if !ch.more {
            break;
        }
    }
    Ok(())
}

/// Make sure every object of `root` is in the local store, downloading
/// what is missing level by level in batches.
pub fn ensure_objects(ws: &Workspace, root: &ObjectId, prompt: bool) -> Result<usize> {
    let mut remote: Option<Remote> = None;
    let mut fetched = 0;
    let mut level = vec![*root];
    while !level.is_empty() {
        fetch_missing(ws, &mut remote, prompt, &level, &mut fetched)?;
        let mut blobs = Vec::new();
        let mut next = Vec::new();
        for id in &level {
            for e in ws.store.tree(id)? {
                if e.mode == Mode::Tree {
                    next.push(e.id);
                } else if !ws.store.has_object(&e.id) {
                    blobs.push(e.id);
                }
            }
        }
        blobs.sort();
        blobs.dedup();
        fetch_missing(ws, &mut remote, prompt, &blobs, &mut fetched)?;
        next.sort();
        next.dedup();
        level = next;
    }
    Ok(fetched)
}

fn fetch_missing(
    ws: &Workspace,
    remote: &mut Option<Remote>,
    prompt: bool,
    ids: &[ObjectId],
    fetched: &mut usize,
) -> Result<()> {
    let want: Vec<ObjectId> = ids
        .iter()
        .filter(|id| !ws.store.has_object(id))
        .copied()
        .collect();
    if want.is_empty() {
        return Ok(());
    }
    if remote.is_none() {
        *remote = Remote::connect(ws, prompt)?;
    }
    let Some(r) = remote.as_ref() else {
        fail!(
            ObjectMissing,
            "checkpoint content ({} object(s)) is not available locally and no server is configured",
            want.len()
        );
    };
    for chunk in want.chunks(2000) {
        let got = r.fetch_batch(chunk)?;
        ws.store.tx(|| {
            for (id, data) in &got {
                ws.store.put_compressed(id, data)?;
            }
            Ok(())
        })?;
        *fetched += got.len();
        if got.len() < chunk.len() {
            let have: HashSet<ObjectId> = got.iter().map(|(id, _)| *id).collect();
            let lost = chunk
                .iter()
                .find(|id| !have.contains(id))
                .expect("missing id");
            fail!(
                ObjectMissing,
                "object {} is not on the server (pruned or never synced)",
                lost.short()
            );
        }
    }
    Ok(())
}

/// Start a detached `git hf sync --background` if configured. Never blocks
/// or fails the caller.
pub fn spawn_background(ws: &Workspace) {
    if std::env::var_os("GHF_BACKGROUND").is_some() {
        return;
    }
    let Some(server) = &ws.cfg.server else { return };
    if !ws.cfg.background_sync().unwrap_or(false) {
        return;
    }
    let Ok(url) = config::normalize_url(&server.url) else {
        return;
    };
    if !matches!(config::trusted(&url), Ok(Some(_))) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let log = ws.store.dir.join("sync.log");
    let Ok(out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    else {
        return;
    };
    let Ok(err) = out.try_clone() else { return };
    use std::os::unix::process::CommandExt;
    let _ = std::process::Command::new(exe)
        .args(["sync", "--background"])
        .current_dir(&ws.repo.root)
        .env("GHF_BACKGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn();
}

/// Body of `git hf sync --background`: one runner at a time, repeat while
/// new checkpoints keep arriving.
pub fn background(ws: &Workspace) -> Result<Option<SyncReport>> {
    let Some(_lock) = FileLock::try_acquire(&ws.store.dir.join("sync.lock"))? else {
        return Ok(None);
    };
    let remote = match Remote::connect(ws, false)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let mut total = SyncReport::default();
    ws.store.maintain(1024 * 1024);
    for _ in 0..10 {
        match sync(ws, &remote) {
            Ok(r) => {
                total.pushed_checkpoints += r.pushed_checkpoints;
                total.pushed_objects += r.pushed_objects;
                total.pending = r.pending;
                if r.pending == 0 {
                    break;
                }
            }
            Err(f) => {
                record_sync_error(ws, &f);
                return Err(f);
            }
        }
    }
    Ok(Some(total))
}

pub fn record_sync_error(ws: &Workspace, f: &Failure) {
    let _ = ws.store.kv_set(
        "last_sync_error",
        &serde_json::json!({
            "at": ghf_proto::id::now_ms(),
            "code": f.code,
            "message": f.message,
        })
        .to_string(),
    );
}
