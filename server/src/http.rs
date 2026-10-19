//! HTTP API v1. See `ghf_proto::api` for the route table.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ghf_proto::api::{
    API_VERSION, ApiError, HoldRequest, ObjectList, PendingReport, PruneRequest, ServerInfo,
};
use ghf_proto::{CheckpointRecord, ErrorCode, Hold, ObjectId, Policy, TraceRecord, TraceState};
use serde::Deserialize;

use crate::prune;
use crate::store::{Error, Store};

pub struct AppState {
    pub store: Mutex<Store>,
    pub server_id: String,
    pub policy: Policy,
    /// Mark UI session cookies `Secure` (the server terminates TLS).
    pub secure_cookies: bool,
}

pub type Shared = Arc<AppState>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self.code.exit_code() {
            ghf_proto::ExitCode::Usage => StatusCode::BAD_REQUEST,
            ghf_proto::ExitCode::NotFound => StatusCode::NOT_FOUND,
            ghf_proto::ExitCode::Refused | ghf_proto::ExitCode::Conflict => StatusCode::CONFLICT,
            _ if self.code == ErrorCode::Auth => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("ghf-server: error: {}", self.message);
        }
        (
            status,
            Json(ApiError {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

pub(crate) type ApiResult<T> = Result<T, Error>;

pub(crate) fn bad(code: ErrorCode, msg: impl Into<String>) -> Error {
    Error::new(code, msg)
}

pub fn router(state: Shared) -> Router {
    let repo = Router::new()
        .route("/objects/missing", post(missing))
        .route("/objects/batch", post(put_batch))
        .route("/objects/fetch", post(fetch_batch))
        .route("/objects/{id}", put(put_object).get(get_object))
        .route("/traces/{id}", put(put_trace).get(get_trace))
        .route(
            "/traces/{id}/holds/{name}",
            put(put_hold).delete(delete_hold),
        )
        .route("/checkpoints/{id}", put(put_checkpoint))
        .route("/changes", get(changes))
        .route("/clients/{id}/pending", put(put_pending))
        .route("/policy", get(policy))
        .route("/prune", post(prune_handler));
    Router::new()
        .route("/v1/info", get(info))
        .merge(crate::ui::router())
        .nest("/v1/repos/{owner}/{name}", repo)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state)
}

async fn info(State(s): State<Shared>) -> Json<ServerInfo> {
    Json(ServerInfo {
        server_id: s.server_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api: API_VERSION.to_owned(),
    })
}

/// Whether a route reads or changes server state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
}

/// Check `token` against `owner/name`. Returns the repository name.
fn check_token(
    s: &AppState,
    token: &str,
    owner: &str,
    name: &str,
    access: Access,
) -> ApiResult<String> {
    let repo = format!("{owner}/{name}");
    if !ghf_proto::api::valid_repo(&repo) {
        return Err(bad(
            ErrorCode::Usage,
            format!("invalid repository {repo:?}"),
        ));
    }
    let info = s
        .store
        .lock()
        .expect("store lock")
        .token(token)?
        .ok_or_else(|| bad(ErrorCode::Auth, "invalid or revoked token"))?;
    if !info.allows(owner, name) {
        return Err(bad(
            ErrorCode::Auth,
            format!("token has no access to {repo}"),
        ));
    }
    if access == Access::Write && info.read_only {
        return Err(bad(ErrorCode::Auth, "token is read-only"));
    }
    Ok(repo)
}

/// Authorize a request for `owner/name`. Returns the repository name.
fn authorize(
    s: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    access: Access,
) -> ApiResult<String> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| bad(ErrorCode::Auth, "missing bearer token"))?;
    check_token(s, token, owner, name, access)
}

fn object_id(s: &str) -> ApiResult<ObjectId> {
    ObjectId::from_hex(s).ok_or_else(|| bad(ErrorCode::Usage, "invalid object id"))
}

async fn missing(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<ObjectList>,
) -> ApiResult<Json<ObjectList>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    let store = s.store.lock().expect("store lock");
    let (present, missing): (Vec<ObjectId>, Vec<ObjectId>) = req
        .ids
        .into_iter()
        .partition(|id| store.has_object(&repo, id));
    // A client that sees "present" will not upload; keep those objects out
    // of the GC grace window until its checkpoint lands.
    store.touch_objects(&repo, &present)?;
    Ok(Json(ObjectList { ids: missing }))
}

async fn put_object(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    let id = object_id(&id)?;
    let s2 = s.clone();
    // Verification and the database write happen off the async runtime.
    tokio::task::spawn_blocking(move || {
        let store = s2.store.lock().expect("store lock");
        store.write_objects(&repo, &[(id, &body[..])])
    })
    .await
    .map_err(|e| bad(ErrorCode::Internal, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn put_batch(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || {
        let objects = ghf_proto::api::unframe(&body)
            .ok_or_else(|| bad(ErrorCode::Usage, "malformed object batch"))?;
        let store = s2.store.lock().expect("store lock");
        store.write_objects(&repo, &objects)
    })
    .await
    .map_err(|e| bad(ErrorCode::Internal, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn fetch_batch(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<ObjectList>,
) -> ApiResult<Response> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Read)?;
    if req.ids.len() > 10_000 {
        return Err(bad(ErrorCode::Usage, "at most 10000 ids per fetch"));
    }
    let store = s.store.lock().expect("store lock");
    let mut body = Vec::new();
    for id in &req.ids {
        if let Some(d) = store.read_object(&repo, id)? {
            ghf_proto::api::frame(&mut body, id, &d);
        }
    }
    Ok(([("content-type", "application/octet-stream")], body).into_response())
}

async fn get_object(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Read)?;
    let id = object_id(&id)?;
    let data = s
        .store
        .lock()
        .expect("store lock")
        .read_object(&repo, &id)?;
    match data {
        Some(d) => Ok(([("content-type", "application/octet-stream")], d).into_response()),
        None => Err(bad(
            ErrorCode::NotFound,
            format!("object {} not found", id.short()),
        )),
    }
}

async fn put_trace(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(t): Json<TraceRecord>,
) -> ApiResult<Json<TraceRecord>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    if t.id != id || !ghf_proto::id::is_id(ghf_proto::id::TRACE, &id) {
        return Err(bad(ErrorCode::Usage, "trace id mismatch"));
    }
    let store = s.store.lock().expect("store lock");
    Ok(Json(store.merge_trace(&repo, &t)?))
}

async fn get_trace(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<TraceRecord>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Read)?;
    let store = s.store.lock().expect("store lock");
    store
        .trace(&repo, &id)?
        .map(Json)
        .ok_or_else(|| bad(ErrorCode::NotFound, format!("trace {id} not found")))
}

async fn put_hold(
    State(s): State<Shared>,
    Path((owner, name, id, hold)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    Json(req): Json<HoldRequest>,
) -> ApiResult<Json<TraceRecord>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    if hold.is_empty()
        || hold.len() > 64
        || !hold
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(bad(ErrorCode::Usage, "invalid hold name"));
    }
    let store = s.store.lock().expect("store lock");
    let mut t = store
        .trace(&repo, &id)?
        .ok_or_else(|| bad(ErrorCode::NotFound, format!("trace {id} not found")))?;
    if t.state == TraceState::Pruned {
        return Err(bad(
            ErrorCode::TraceState,
            format!("trace {id} is already pruned"),
        ));
    }
    t.holds.retain(|h| h.name != hold);
    t.holds.push(Hold {
        name: hold,
        expires_at: req.expires_at,
        created_at: ghf_proto::id::now_ms(),
    });
    store.save_trace(&repo, &t)?;
    Ok(Json(t))
}

async fn delete_hold(
    State(s): State<Shared>,
    Path((owner, name, id, hold)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<TraceRecord>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    let store = s.store.lock().expect("store lock");
    let mut t = store
        .trace(&repo, &id)?
        .ok_or_else(|| bad(ErrorCode::NotFound, format!("trace {id} not found")))?;
    let before = t.holds.len();
    t.holds.retain(|h| h.name != hold);
    if t.holds.len() != before {
        store.save_trace(&repo, &t)?;
    }
    Ok(Json(t))
}

async fn put_checkpoint(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(c): Json<CheckpointRecord>,
) -> ApiResult<StatusCode> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    if c.id != id || !ghf_proto::id::is_id(ghf_proto::id::CHECKPOINT, &id) {
        return Err(bad(ErrorCode::Usage, "checkpoint id mismatch"));
    }
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<()> {
        let store = s2.store.lock().expect("store lock");
        if store.has_checkpoint(&repo, &c.id)? {
            return Ok(());
        }
        let t = store.trace(&repo, &c.trace_id)?.ok_or_else(|| {
            bad(
                ErrorCode::NotFound,
                format!("trace {} not found; push it first", c.trace_id),
            )
        })?;
        if t.state == TraceState::Pruned {
            return Err(bad(
                ErrorCode::TraceState,
                format!("trace {} is pruned", t.id),
            ));
        }
        for root in c.roots() {
            if let Some(m) = store.missing_in_closure(&repo, &root)? {
                return Err(bad(
                    ErrorCode::ObjectMissing,
                    format!("object {} of checkpoint {} is missing", m.short(), c.id),
                ));
            }
        }
        store.insert_checkpoint(&repo, &c)
    })
    .await
    .map_err(|e| bad(ErrorCode::Internal, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: i64,
}

async fn changes(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<Since>,
    headers: HeaderMap,
) -> ApiResult<Json<ghf_proto::api::Changes>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Read)?;
    let store = s.store.lock().expect("store lock");
    Ok(Json(store.changes(&repo, q.since, 500)?))
}

async fn put_pending(
    State(s): State<Shared>,
    Path((owner, name, client)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(req): Json<PendingReport>,
) -> ApiResult<StatusCode> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    if client.is_empty() || client.len() > 64 {
        return Err(bad(ErrorCode::Usage, "invalid client id"));
    }
    s.store
        .lock()
        .expect("store lock")
        .set_pending(&repo, &client, &req.traces)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn policy(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Policy>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Read)?;
    Ok(Json(
        s.store
            .lock()
            .expect("store lock")
            .policy(&repo, &s.policy)?,
    ))
}

async fn prune_handler(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<PruneRequest>,
) -> ApiResult<Json<ghf_proto::api::PruneReport>> {
    let repo = authorize(&s, &headers, &owner, &name, Access::Write)?;
    let s2 = s.clone();
    let report = tokio::task::spawn_blocking(move || {
        let store = s2.store.lock().expect("store lock");
        prune::prune_repo(&store, &repo, &s2.policy, req.dry_run)
    })
    .await
    .map_err(|e| bad(ErrorCode::Internal, e.to_string()))??;
    Ok(Json(report))
}
