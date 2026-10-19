//! Read-only web UI under `/ui`: trace lists, timelines, diffs and files at
//! a checkpoint. Server-rendered HTML, no scripts, no write actions.
//!
//! Browsers sign in by pasting a token once; it is kept in an HttpOnly,
//! SameSite=Strict cookie scoped to `/ui`. Repository access follows the
//! token's scopes exactly as the API does.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use ghf_proto::object::{self, Kind, Mode, TreeEntry};
use ghf_proto::{ChangeStatus, CheckpointRecord, ObjectId, TraceRecord, TraceState};
use serde::Deserialize;

use crate::http::Shared;
use crate::store::{Store, TokenInfo};

const COOKIE: &str = "ghf_session";
const COOKIE_MAX_AGE: u64 = 30 * 24 * 3600;
/// Files larger than this are not shown inline.
const MAX_INLINE: u64 = 1024 * 1024;
/// Stop rendering a diff after this many lines.
const MAX_DIFF_LINES: usize = 20_000;

const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; \
                   form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/ui", get(home))
        .route("/ui/style.css", get(style))
        .route("/ui/login", get(login_page).post(login))
        .route("/ui/logout", post(logout))
        .route("/ui/{owner}/{name}", get(repo_page))
        .route("/ui/{owner}/{name}/traces/{id}", get(trace_page))
        .route("/ui/{owner}/{name}/checkpoints/{id}", get(checkpoint_page))
        .route("/ui/{owner}/{name}/checkpoints/{id}/files", get(files_root))
        .route(
            "/ui/{owner}/{name}/checkpoints/{id}/files/{*path}",
            get(files_page),
        )
        .route("/ui/{owner}/{name}/diff", get(diff_page))
        .layer(axum::middleware::map_response(security_headers))
}

async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    r
}

// ---------------------------------------------------------------------------
// errors and responses

enum UiError {
    /// Not signed in: send the browser to the login form.
    Login(String),
    /// A cross-site navigation arrived without the SameSite=Strict cookie;
    /// reload once from our own origin so the cookie is sent.
    Reload(String),
    Page(StatusCode, String),
}

type UiResult<T> = Result<T, UiError>;

fn not_found(what: impl Into<String>) -> UiError {
    UiError::Page(StatusCode::NOT_FOUND, what.into())
}

impl From<crate::store::Error> for UiError {
    fn from(e: crate::store::Error) -> Self {
        eprintln!("ghf-server: ui: {}", e.message);
        UiError::Page(StatusCode::INTERNAL_SERVER_ERROR, e.message)
    }
}

impl From<std::io::Error> for UiError {
    fn from(e: std::io::Error) -> Self {
        crate::store::Error::from(e).into()
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        match self {
            UiError::Login(next) => see_other(&format!("/ui/login?next={}", enc(&next))),
            UiError::Reload(to) => {
                let body = format!(
                    "<!doctype html><meta charset=\"utf-8\">\
                     <meta http-equiv=\"refresh\" content=\"0;url={0}\">\
                     <title>ghf</title><a href=\"{0}\">Continue</a>",
                    esc(&to)
                );
                html(StatusCode::OK, body)
            }
            UiError::Page(status, msg) => {
                let body = layout(
                    status.canonical_reason().unwrap_or("Error"),
                    &[],
                    false,
                    &format!(
                        "<h1>{}</h1><p>{}</p><p><a href=\"/ui\">Back to repositories</a></p>",
                        status.as_u16(),
                        esc(&msg)
                    ),
                );
                html(status, body)
            }
        }
    }
}

fn html(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn see_other(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_owned())]).into_response()
}

// ---------------------------------------------------------------------------
// sessions

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })
        .filter(|t| !t.is_empty())
}

/// The signed-in token, or a redirect to the login form.
fn viewer(s: &Shared, headers: &HeaderMap, uri: &Uri) -> UiResult<TokenInfo> {
    let here = uri
        .path_and_query()
        .map_or("/ui".to_owned(), |p| p.as_str().to_owned());
    let Some(token) = session_token(headers) else {
        let cross_site = headers
            .get("sec-fetch-site")
            .is_some_and(|v| v.as_bytes() == b"cross-site");
        return Err(if cross_site {
            UiError::Reload(here)
        } else {
            UiError::Login(here)
        });
    };
    s.store
        .lock()
        .expect("store lock")
        .token(&token)?
        .ok_or(UiError::Login(here))
}

/// Check that the token sees `owner/name`. Out-of-scope repositories look
/// exactly like missing ones.
fn repo_for(info: &TokenInfo, owner: &str, name: &str) -> UiResult<String> {
    let repo = format!("{owner}/{name}");
    if !ghf_proto::api::valid_repo(&repo) || !info.allows(owner, name) {
        return Err(not_found(format!(
            "No repository {repo}, or your token cannot see it."
        )));
    }
    Ok(repo)
}

/// Run page rendering off the async runtime with the store locked.
async fn render<F>(s: Shared, f: F) -> Response
where
    F: FnOnce(&Store) -> UiResult<String> + Send + 'static,
{
    let r = tokio::task::spawn_blocking(move || {
        let store = s.store.lock().expect("store lock");
        f(&store)
    })
    .await;
    match r {
        Ok(Ok(body)) => html(StatusCode::OK, body),
        Ok(Err(e)) => e.into_response(),
        Err(e) => UiError::Page(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Only same-origin form posts may sign in or out. Browsers that send
/// `Sec-Fetch-Site` are trusted on it; otherwise `Origin` must match `Host`.
fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site.as_bytes() == b"same-origin";
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|o| o == host)
}

/// A post-login target: only paths inside the UI.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if (n == "/ui" || n.starts_with("/ui/") || n.starts_with("/ui?"))
                && !n.contains(['\\', '\r', '\n'])
                && !n.contains("//") =>
        {
            n.to_owned()
        }
        _ => "/ui".to_owned(),
    }
}

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

async fn login_page(Query(q): Query<NextQuery>) -> Response {
    html(
        StatusCode::OK,
        login_html(&safe_next(q.next.as_deref()), None),
    )
}

fn login_html(next: &str, error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", esc(e)))
        .unwrap_or_default();
    layout(
        "Sign in",
        &[],
        false,
        &format!(
            "<h1>Sign in</h1>\
             <p>Paste a ghf-server token. A read-only token is enough: \
             <code>ghf-server token create --read-only</code>.</p>{err}\
             <form method=\"post\" action=\"/ui/login\" class=\"login\">\
             <input type=\"hidden\" name=\"next\" value=\"{}\">\
             <label for=\"token\">Token</label>\
             <input id=\"token\" name=\"token\" type=\"password\" autocomplete=\"off\" \
             spellcheck=\"false\" required autofocus>\
             <button type=\"submit\">Sign in</button></form>\
             <p class=\"muted\">The token is kept in an HttpOnly cookie for 30 days. \
             This viewer never changes anything on the server.</p>",
            esc(next)
        ),
    )
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
    next: Option<String>,
}

async fn login(State(s): State<Shared>, headers: HeaderMap, Form(f): Form<LoginForm>) -> Response {
    if !same_origin(&headers) {
        return UiError::Page(
            StatusCode::FORBIDDEN,
            "Cross-origin sign-in refused.".into(),
        )
        .into_response();
    }
    let next = safe_next(f.next.as_deref());
    let token = f.token.trim();
    let well_formed = !token.is_empty()
        && token.len() <= 256
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let valid = well_formed
        && match s.store.lock().expect("store lock").token(token) {
            Ok(t) => t.is_some(),
            Err(e) => return UiError::from(e).into_response(),
        };
    if !valid {
        return html(
            StatusCode::UNAUTHORIZED,
            login_html(&next, Some("That token is invalid or revoked.")),
        );
    }
    let mut r = see_other(&next);
    let cookie = format!(
        "{COOKIE}={token}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE}{}",
        if secure(&s, &headers) { "; Secure" } else { "" }
    );
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is ASCII"),
    );
    r
}

async fn logout(State(s): State<Shared>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return UiError::Page(
            StatusCode::FORBIDDEN,
            "Cross-origin sign-out refused.".into(),
        )
        .into_response();
    }
    let mut r = see_other("/ui/login");
    let cookie = format!(
        "{COOKIE}=; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure(&s, &headers) { "; Secure" } else { "" }
    );
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is ASCII"),
    );
    r
}

fn secure(s: &Shared, headers: &HeaderMap) -> bool {
    s.secure_cookies
        || headers
            .get("x-forwarded-proto")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"))
}

async fn style() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        STYLE,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// pages

async fn home(State(s): State<Shared>, headers: HeaderMap, uri: Uri) -> Response {
    let info = match viewer(&s, &headers, &uri) {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    render(s, move |store| {
        let mut repos: Vec<String> = store
            .repos()?
            .into_iter()
            .filter(|r| r.split_once('/').is_some_and(|(o, n)| info.allows(o, n)))
            .collect();
        repos.sort();
        let mut b = String::from("<h1>Repositories</h1>");
        if !info.read_only {
            b.push_str(
                "<p class=\"note\">You are signed in with a token that can write. \
                 The viewer never writes, but a read-only token \
                 (<code>ghf-server token create --read-only</code>) is safer in a browser.</p>",
            );
        }
        if repos.is_empty() {
            b.push_str("<p>No repositories with traces are visible to this token.</p>");
        } else {
            b.push_str("<ul class=\"repos\">");
            for r in &repos {
                let _ = write!(b, "<li><a href=\"/ui/{}\">{}</a></li>", enc_path(r), esc(r));
            }
            b.push_str("</ul>");
        }
        Ok(layout("Repositories", &[], true, &b))
    })
    .await
}

fn last_activity(t: &TraceRecord, last_ck: Option<i64>) -> i64 {
    [
        Some(t.created_at),
        t.closed_at,
        t.finalized_at,
        t.pruned_at,
        last_ck,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0)
}

async fn repo_page(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let repo = match viewer(&s, &headers, &uri).and_then(|i| repo_for(&i, &owner, &name)) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    render(s, move |store| {
        let stats = store.trace_stats(&repo)?;
        let mut traces = store.traces(&repo)?;
        let activity = |t: &TraceRecord| {
            last_activity(t, stats.get(&t.id).and_then(|s| s.last_checkpoint_at))
        };
        traces.sort_by_key(|t| std::cmp::Reverse(activity(t)));
        let base = repo_base(&repo);
        let mut b = format!("<h1>{}</h1>", esc(&repo));
        if traces.is_empty() {
            b.push_str("<p>No traces yet.</p>");
        } else {
            b.push_str(
                "<div class=\"scroll\"><table class=\"list\"><caption>Traces, most recent first</caption>\
                 <thead><tr><th scope=\"col\">Trace</th><th scope=\"col\">State</th>\
                 <th scope=\"col\">Writer</th><th scope=\"col\" class=\"num\">Checkpoints</th>\
                 <th scope=\"col\">Last activity</th><th scope=\"col\">Final commit</th></tr></thead><tbody>",
            );
            for t in &traces {
                let st = stats.get(&t.id).cloned().unwrap_or_default();
                let fork = t
                    .forked_from
                    .as_ref()
                    .map(|f| {
                        format!(
                            "<div class=\"muted small\">forked from <a href=\"{base}/checkpoints/{}\"><code>{}</code></a></div>",
                            enc(f),
                            esc(f)
                        )
                    })
                    .unwrap_or_default();
                let _ = write!(
                    b,
                    "<tr><td><a href=\"{base}/traces/{}\">{}</a>{}{fork}</td><td>{}</td><td>{}</td>\
                     <td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                    enc(&t.id),
                    trace_label(t),
                    if t.name.is_some() {
                        format!("<div class=\"muted small\"><code>{}</code></div>", esc(&t.id))
                    } else {
                        String::new()
                    },
                    state_badge(t),
                    if st.authors.is_empty() {
                        "<span class=\"muted\">none</span>".to_owned()
                    } else {
                        esc(&st.authors.join(", "))
                    },
                    st.checkpoints,
                    time_html(activity(t)),
                    t.final_commit
                        .as_deref()
                        .map(|c| format!("<code title=\"{0}\">{1}</code>", esc(c), esc(&c[..c.len().min(12)])))
                        .unwrap_or_else(|| "<span class=\"muted\">—</span>".into()),
                );
            }
            b.push_str("</tbody></table></div>");
        }
        Ok(layout(&repo, &[(esc(&repo), None)], true, &b))
    })
    .await
}

async fn trace_page(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let repo = match viewer(&s, &headers, &uri).and_then(|i| repo_for(&i, &owner, &name)) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    render(s, move |store| {
        let t = store
            .trace(&repo, &id)?
            .ok_or_else(|| not_found(format!("No trace {id} in {repo}.")))?;
        let cks = store.trace_checkpoints(&repo, &id)?;
        let base = repo_base(&repo);
        let title = t.name.clone().unwrap_or_else(|| t.id.clone());
        let mut b = format!("<h1>Trace {}</h1>", trace_label(&t));

        // Summary.
        b.push_str("<dl class=\"meta\">");
        meta(&mut b, "Id", &format!("<code>{}</code>", esc(&t.id)));
        meta(&mut b, "State", &state_badge(&t));
        meta(&mut b, "Created", &time_html(t.created_at));
        if let Some(f) = &t.forked_from {
            let link = match store.checkpoint(&repo, f)? {
                Some(c) => format!(
                    "checkpoint <a href=\"{base}/checkpoints/{0}\"><code>{1}</code></a> \
                     (#{2} of trace <a href=\"{base}/traces/{3}\"><code>{4}</code></a>)",
                    enc(f),
                    esc(f),
                    c.seq,
                    enc(&c.trace_id),
                    esc(&c.trace_id)
                ),
                None => format!(
                    "<code>{}</code> <span class=\"muted\">(pruned)</span>",
                    esc(f)
                ),
            };
            meta(&mut b, "Forked from", &link);
        }
        if let Some(at) = t.closed_at {
            meta(&mut b, "Closed", &time_html(at));
        }
        if let Some(at) = t.finalized_at {
            meta(&mut b, "Finalized", &time_html(at));
        }
        if let Some(c) = &t.final_commit {
            meta(&mut b, "Final commit", &format!("<code>{}</code>", esc(c)));
        }
        if t.abandoned {
            meta(&mut b, "Outcome", "abandoned");
        }
        if let Some(at) = t.pruned_at {
            meta(&mut b, "Pruned", &time_html(at));
        }
        if !t.holds.is_empty() {
            let holds: Vec<String> = t.holds.iter().map(|h| esc(&h.name)).collect();
            meta(&mut b, "Holds", &holds.join(", "));
        }
        meta(
            &mut b,
            "Context",
            &format!("<code>{}</code>", esc(&t.context_id)),
        );
        b.push_str("</dl>");

        // Traces forked from this one.
        let ids: Vec<&str> = cks.iter().map(|c| c.id.as_str()).collect();
        let forks: Vec<TraceRecord> = store
            .traces(&repo)?
            .into_iter()
            .filter(|f| f.forked_from.as_deref().is_some_and(|x| ids.contains(&x)))
            .collect();
        if !forks.is_empty() {
            b.push_str("<h2>Forks</h2><ul>");
            for f in &forks {
                let _ = write!(
                    b,
                    "<li><a href=\"{base}/traces/{}\">{}</a> from <code>{}</code> {}</li>",
                    enc(&f.id),
                    trace_label(f),
                    esc(f.forked_from.as_deref().unwrap_or("")),
                    state_badge(f)
                );
            }
            b.push_str("</ul>");
        }

        // Compare form.
        if cks.len() >= 2 {
            let opts = |sel: &str| {
                cks.iter()
                    .map(|c| {
                        format!(
                            "<option value=\"{0}\"{2}>#{1} {3}</option>",
                            esc(&c.id),
                            c.seq,
                            if c.id == sel { " selected" } else { "" },
                            esc(&truncate(c.message.as_deref().unwrap_or(&c.id), 60))
                        )
                    })
                    .collect::<String>()
            };
            let _ = write!(
                b,
                "<form method=\"get\" action=\"{base}/diff\" class=\"compare\">\
                 <label for=\"from\">Compare</label><select id=\"from\" name=\"from\">{}</select>\
                 <label for=\"to\">with</label><select id=\"to\" name=\"to\">{}</select>\
                 <button type=\"submit\">Show diff</button></form>",
                opts(&cks[0].id),
                opts(&cks[cks.len() - 1].id)
            );
        }

        // Timeline.
        let _ = write!(b, "<h2>Checkpoints ({})</h2>", cks.len());
        if cks.is_empty() {
            b.push_str("<p>No checkpoints on the server for this trace.</p>");
        }
        b.push_str("<ol class=\"timeline\">");
        for c in &cks {
            b.push_str(&checkpoint_card(&base, c));
        }
        b.push_str("</ol>");
        Ok(layout(
            &format!("Trace {title}"),
            &[
                (esc(&repo), Some(base.clone())),
                (format!("trace <code>{}</code>", esc(&t.id)), None),
            ],
            true,
            &b,
        ))
    })
    .await
}

fn checkpoint_card(base: &str, c: &CheckpointRecord) -> String {
    let outcome = match c.annotations.get("tests").map(String::as_str) {
        Some("pass") => " pass",
        Some("fail") => " fail",
        _ => "",
    };
    let mut b = format!(
        "<li class=\"checkpoint{outcome}\" id=\"{}\"><div class=\"head\">\
         <a class=\"seq\" href=\"{base}/checkpoints/{}\">#{}</a> <span class=\"msg\">{}</span></div>\
         <div class=\"muted small\">{} · {} · <code>{}</code> · {} file(s), {}</div>",
        esc(&c.id),
        enc(&c.id),
        c.seq,
        c.message
            .as_deref()
            .map(esc)
            .unwrap_or_else(|| "<span class=\"muted\">no message</span>".into()),
        time_html(c.created_at),
        esc(&c.author),
        esc(&c.id),
        c.files,
        human_bytes(c.bytes),
    );
    if !c.annotations.is_empty() {
        b.push_str("<ul class=\"annotations\" aria-label=\"Annotations\">");
        for (k, v) in &c.annotations {
            let cls = if k == "tests" && (v == "pass" || v == "fail") {
                format!(" {v}")
            } else {
                String::new()
            };
            let _ = write!(b, "<li class=\"badge{cls}\">{}={}</li>", esc(k), esc(v));
        }
        b.push_str("</ul>");
    }
    b.push_str(&changes_list(base, c));
    let _ = write!(
        b,
        "<div class=\"actions small\"><a href=\"{base}/checkpoints/{0}\">Diff vs parent</a> · \
         <a href=\"{base}/checkpoints/{0}/files\">Browse files</a></div></li>",
        enc(&c.id)
    );
    b
}

fn changes_list(base: &str, c: &CheckpointRecord) -> String {
    if c.changes.is_empty() && c.skipped.is_empty() {
        return "<p class=\"small muted\">No file changes.</p>".into();
    }
    let mut b = String::from("<ul class=\"changes\">");
    for ch in &c.changes {
        let (label, cls) = match ch.status {
            ChangeStatus::Added => ("A", "add"),
            ChangeStatus::Modified => ("M", "mod"),
            ChangeStatus::Deleted => ("D", "del"),
            ChangeStatus::Renamed => ("R", "mod"),
            ChangeStatus::Mode => ("T", "mod"),
        };
        let path = if ch.status == ChangeStatus::Deleted {
            format!("<span class=\"path\">{}</span>", esc(&ch.path))
        } else {
            file_link(base, &c.id, &ch.path)
        };
        let from = ch
            .from
            .as_deref()
            .map(|f| format!("<span class=\"muted\">{} → </span>", esc(f)))
            .unwrap_or_default();
        let _ = write!(
            b,
            "<li><span class=\"st {cls}\" title=\"{}\">{label}</span> {from}{path}</li>",
            status_word(ch.status)
        );
    }
    for sk in &c.skipped {
        let _ = write!(
            b,
            "<li><span class=\"st skip\" title=\"skipped\">S</span> <span class=\"path\">{}</span> \
             <span class=\"muted\">skipped: {}</span></li>",
            esc(&sk.path),
            esc(&sk.reason)
        );
    }
    b.push_str("</ul>");
    b
}

fn status_word(s: ChangeStatus) -> &'static str {
    match s {
        ChangeStatus::Added => "added",
        ChangeStatus::Modified => "modified",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Renamed => "renamed",
        ChangeStatus::Mode => "mode changed",
    }
}

async fn checkpoint_page(
    State(s): State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let repo = match viewer(&s, &headers, &uri).and_then(|i| repo_for(&i, &owner, &name)) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    render(s, move |store| {
        let c = load_checkpoint(store, &repo, &id)?;
        let base = repo_base(&repo);
        let mut b = format!(
            "<h1>Checkpoint #{} of trace <a href=\"{base}/traces/{}\"><code>{}</code></a></h1>",
            c.seq,
            enc(&c.trace_id),
            esc(&c.trace_id)
        );
        b.push_str("<ol class=\"timeline single\">");
        b.push_str(&checkpoint_card(&base, &c));
        b.push_str("</ol>");
        let parent = match &c.parent {
            Some(p) => match store.checkpoint(&repo, p)? {
                Some(pc) => Some(pc),
                None => {
                    let _ = write!(
                        b,
                        "<p class=\"note\">The parent checkpoint <code>{}</code> is no longer on \
                         the server, so there is no diff to show.</p>",
                        esc(p)
                    );
                    return Ok(layout(
                        &format!("Checkpoint {}", c.id),
                        &crumbs(&repo, &base, &c),
                        true,
                        &b,
                    ));
                }
            },
            None => None,
        };
        let _ = write!(
            b,
            "<h2>Diff against {}</h2>",
            match &parent {
                Some(p) => format!(
                    "parent <a href=\"{base}/checkpoints/{}\">#{} <code>{}</code></a>",
                    enc(&p.id),
                    p.seq,
                    esc(&p.id)
                ),
                None => "an empty tree (first checkpoint)".into(),
            }
        );
        b.push_str(&diff_html(store, &repo, &base, parent.as_ref(), &c)?);
        Ok(layout(
            &format!("Checkpoint {}", c.id),
            &crumbs(&repo, &base, &c),
            true,
            &b,
        ))
    })
    .await
}

fn crumbs(repo: &str, base: &str, c: &CheckpointRecord) -> Vec<(String, Option<String>)> {
    vec![
        (esc(repo), Some(base.to_owned())),
        (
            format!("trace <code>{}</code>", esc(&c.trace_id)),
            Some(format!("{base}/traces/{}", enc(&c.trace_id))),
        ),
        (format!("#{}", c.seq), None),
    ]
}

#[derive(Deserialize)]
struct DiffQuery {
    from: Option<String>,
    to: Option<String>,
}

async fn diff_page(
    State(s): State<Shared>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<DiffQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let repo = match viewer(&s, &headers, &uri).and_then(|i| repo_for(&i, &owner, &name)) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let (Some(from), Some(to)) = (q.from, q.to) else {
        return UiError::Page(
            StatusCode::BAD_REQUEST,
            "A diff needs both ?from= and ?to= checkpoint ids.".into(),
        )
        .into_response();
    };
    render(s, move |store| {
        let a = load_checkpoint(store, &repo, &from)?;
        let c = load_checkpoint(store, &repo, &to)?;
        let base = repo_base(&repo);
        let side = |c: &CheckpointRecord| {
            format!(
                "<a href=\"{base}/checkpoints/{}\">#{} <code>{}</code></a> of trace \
                 <a href=\"{base}/traces/{}\"><code>{}</code></a>",
                enc(&c.id),
                c.seq,
                esc(&c.id),
                enc(&c.trace_id),
                esc(&c.trace_id)
            )
        };
        let mut b = format!(
            "<h1>Diff</h1><p>From {} to {} · <a href=\"{base}/diff?from={}&amp;to={}\">swap</a></p>",
            side(&a),
            side(&c),
            enc(&c.id),
            enc(&a.id)
        );
        b.push_str(&diff_html(store, &repo, &base, Some(&a), &c)?);
        Ok(layout(
            "Diff",
            &[(esc(&repo), Some(base.clone())), ("diff".into(), None)],
            true,
            &b,
        ))
    })
    .await
}

fn load_checkpoint(store: &Store, repo: &str, id: &str) -> UiResult<CheckpointRecord> {
    store.checkpoint(repo, id)?.ok_or_else(|| {
        not_found(format!(
            "No checkpoint {id} in {repo}; it may have been pruned."
        ))
    })
}

async fn files_root(
    s: State<Shared>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    files(s, owner, name, id, String::new(), headers, uri).await
}

async fn files_page(
    s: State<Shared>,
    Path((owner, name, id, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    files(s, owner, name, id, path, headers, uri).await
}

async fn files(
    State(s): State<Shared>,
    owner: String,
    name: String,
    id: String,
    path: String,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let repo = match viewer(&s, &headers, &uri).and_then(|i| repo_for(&i, &owner, &name)) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    render(s, move |store| {
        let c = load_checkpoint(store, &repo, &id)?;
        let base = repo_base(&repo);
        let path = path.trim_matches('/').to_owned();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        // Walk the manifest down to `path`.
        let mut node = (Mode::Tree, c.root, 0u64);
        for p in &parts {
            if node.0 != Mode::Tree {
                return Err(not_found(format!("{path} is not in checkpoint {id}.")));
            }
            let entries = tree(store, &repo, &node.1)?;
            let e = entries
                .into_iter()
                .find(|e| e.name == *p)
                .ok_or_else(|| not_found(format!("{path} is not in checkpoint {id}.")))?;
            node = (e.mode, e.id, e.size);
        }
        let ck_url = format!("{base}/checkpoints/{}", enc(&c.id));
        let mut crumbs_html = format!("<a href=\"{ck_url}/files\">/</a>");
        let mut acc = String::new();
        for (i, p) in parts.iter().enumerate() {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            if i + 1 == parts.len() {
                let _ = write!(crumbs_html, "{}{}", if i > 0 { "/" } else { "" }, esc(p));
            } else {
                let _ = write!(
                    crumbs_html,
                    "{}<a href=\"{ck_url}/files/{}\">{}</a>",
                    if i > 0 { "/" } else { "" },
                    enc_path(&acc),
                    esc(p)
                );
            }
        }
        let mut b = format!(
            "<h1 class=\"path\">{crumbs_html}</h1><p class=\"muted\">At checkpoint \
             <a href=\"{ck_url}\">#{} <code>{}</code></a> · {}</p>",
            c.seq,
            esc(&c.id),
            time_html(c.created_at)
        );
        if node.0 == Mode::Tree {
            let mut entries = tree(store, &repo, &node.1)?;
            entries.sort_by(|a, b| {
                (a.mode != Mode::Tree, &a.name).cmp(&(b.mode != Mode::Tree, &b.name))
            });
            b.push_str(
                "<table class=\"list\"><thead><tr><th scope=\"col\">Name</th>\
                 <th scope=\"col\" class=\"num\">Size</th></tr></thead><tbody>",
            );
            for e in &entries {
                let p = if path.is_empty() {
                    e.name.clone()
                } else {
                    format!("{path}/{}", e.name)
                };
                let _ = write!(
                    b,
                    "<tr><td><a href=\"{ck_url}/files/{}\">{}{}</a>{}</td><td class=\"num\">{}</td></tr>",
                    enc_path(&p),
                    esc(&e.name),
                    if e.mode == Mode::Tree { "/" } else { "" },
                    match e.mode {
                        Mode::Exec => " <span class=\"muted small\">executable</span>",
                        Mode::Symlink => " <span class=\"muted small\">symlink</span>",
                        _ => "",
                    },
                    if e.mode == Mode::Tree {
                        String::new()
                    } else {
                        human_bytes(e.size)
                    }
                );
            }
            b.push_str("</tbody></table>");
        } else {
            let _ = write!(
                b,
                "<p class=\"muted small\">{}{}</p>",
                human_bytes(node.2),
                match node.0 {
                    Mode::Exec => " · executable",
                    Mode::Symlink => " · symlink",
                    _ => "",
                }
            );
            if node.2 > MAX_INLINE {
                b.push_str("<p class=\"note\">This file is too large to show here.</p>");
            } else {
                let data = blob(store, &repo, &node.1)?;
                if is_binary(&data) {
                    b.push_str("<p class=\"note\">Binary file, not shown.</p>");
                } else {
                    b.push_str("<div class=\"scroll\"><table class=\"code\"><tbody>");
                    for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
                        let _ = write!(
                            b,
                            "<tr><td class=\"ln\">{}</td><td class=\"src\">{}</td></tr>",
                            i + 1,
                            esc(line)
                        );
                    }
                    b.push_str("</tbody></table></div>");
                }
            }
        }
        let mut cr = crumbs(&repo, &base, &c);
        cr.last_mut().expect("crumb").1 = Some(ck_url.clone());
        cr.push((
            if path.is_empty() {
                "files".into()
            } else {
                format!("<span class=\"path\">{}</span>", esc(&path))
            },
            None,
        ));
        Ok(layout(
            &if path.is_empty() { "Files".into() } else { path.clone() },
            &cr,
            true,
            &b,
        ))
    })
    .await
}

// ---------------------------------------------------------------------------
// objects and diffs

fn tree(store: &Store, repo: &str, id: &ObjectId) -> UiResult<Vec<TreeEntry>> {
    store.tree(repo, id)?.ok_or_else(|| {
        not_found(format!(
            "Tree {} is not on the server; the checkpoint may have been pruned.",
            id.short()
        ))
    })
}

fn blob(store: &Store, repo: &str, id: &ObjectId) -> UiResult<Vec<u8>> {
    let data = store.read_object(repo, id)?.ok_or_else(|| {
        not_found(format!(
            "Object {} is not on the server; the checkpoint may have been pruned.",
            id.short()
        ))
    })?;
    let raw = object::decompress(&data)?;
    match object::parse_raw(&raw) {
        Some((Kind::Blob, body)) => Ok(body.to_vec()),
        _ => Err(not_found(format!("Object {} is not a file.", id.short()))),
    }
}

/// Same rule as the client's `trace diff`.
fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0) || std::str::from_utf8(b).is_err()
}

struct Delta {
    path: String,
    old: Option<TreeEntry>,
    new: Option<TreeEntry>,
}

/// Changed files between two trees, skipping identical subtrees.
fn diff_trees(
    store: &Store,
    repo: &str,
    a: Option<&ObjectId>,
    b: Option<&ObjectId>,
    prefix: &str,
    out: &mut Vec<Delta>,
) -> UiResult<()> {
    if a.is_some() && a == b {
        return Ok(());
    }
    let load = |id: Option<&ObjectId>| -> UiResult<BTreeMap<String, TreeEntry>> {
        Ok(match id {
            Some(id) => tree(store, repo, id)?
                .into_iter()
                .map(|e| (e.name.clone(), e))
                .collect(),
            None => BTreeMap::new(),
        })
    };
    let (mut old, new) = (load(a)?, load(b)?);
    let mut names: Vec<String> = old.keys().chain(new.keys()).cloned().collect();
    names.sort();
    names.dedup();
    for n in names {
        let path = format!("{prefix}{n}");
        let o = old.remove(&n);
        let nw = new.get(&n).cloned();
        let o_tree = o.as_ref().filter(|e| e.mode == Mode::Tree).map(|e| e.id);
        let n_tree = nw.as_ref().filter(|e| e.mode == Mode::Tree).map(|e| e.id);
        if o_tree.is_some() || n_tree.is_some() {
            diff_trees(
                store,
                repo,
                o_tree.as_ref(),
                n_tree.as_ref(),
                &format!("{path}/"),
                out,
            )?;
        }
        let o_file = o.filter(|e| e.mode != Mode::Tree);
        let n_file = nw.filter(|e| e.mode != Mode::Tree);
        let same = match (&o_file, &n_file) {
            (Some(x), Some(y)) => x.id == y.id && x.mode == y.mode,
            (None, None) => true,
            _ => false,
        };
        if !same {
            out.push(Delta {
                path,
                old: o_file,
                new: n_file,
            });
        }
    }
    Ok(())
}

fn diff_html(
    store: &Store,
    repo: &str,
    base: &str,
    from: Option<&CheckpointRecord>,
    to: &CheckpointRecord,
) -> UiResult<String> {
    let mut deltas = Vec::new();
    diff_trees(
        store,
        repo,
        from.map(|c| &c.root),
        Some(&to.root),
        "",
        &mut deltas,
    )?;
    if deltas.is_empty() {
        return Ok("<p>No differences.</p>".into());
    }
    let mut b = format!(
        "<p class=\"muted\">{} file(s) changed.</p><ul class=\"changes toc\">",
        deltas.len()
    );
    for (i, d) in deltas.iter().enumerate() {
        let (label, cls, word) = match (&d.old, &d.new) {
            (None, _) => ("A", "add", "added"),
            (_, None) => ("D", "del", "deleted"),
            (Some(o), Some(n)) if o.id == n.id => ("T", "mod", "mode changed"),
            _ => ("M", "mod", "modified"),
        };
        let _ = write!(
            b,
            "<li><span class=\"st {cls}\" title=\"{word}\">{label}</span> <a href=\"#f{i}\">{}</a></li>",
            esc(&d.path)
        );
    }
    b.push_str("</ul>");
    let mut budget = MAX_DIFF_LINES;
    for (i, d) in deltas.iter().enumerate() {
        let view = match (&d.new, from) {
            (Some(_), _) => format!(
                " <a class=\"small\" href=\"{base}/checkpoints/{}/files/{}\">view file</a>",
                enc(&to.id),
                enc_path(&d.path)
            ),
            (None, Some(f)) => format!(
                " <a class=\"small\" href=\"{base}/checkpoints/{}/files/{}\">view old file</a>",
                enc(&f.id),
                enc_path(&d.path)
            ),
            (None, None) => String::new(),
        };
        let _ = write!(
            b,
            "<section class=\"file\" id=\"f{i}\"><h3><span class=\"path\">{}</span>{view}</h3>",
            esc(&d.path)
        );
        if let (Some(o), Some(n)) = (&d.old, &d.new)
            && o.mode != n.mode
        {
            let _ = write!(
                b,
                "<p class=\"small muted\">mode {} → {}</p>",
                mode_word(o.mode),
                mode_word(n.mode)
            );
        }
        if budget == 0 {
            b.push_str("<p class=\"note\">Diff truncated; too many lines.</p></section>");
            continue;
        }
        let size = |e: &Option<TreeEntry>| e.as_ref().map_or(0, |e| e.size);
        if size(&d.old) > MAX_INLINE || size(&d.new) > MAX_INLINE {
            b.push_str("<p class=\"note\">File too large to diff here.</p></section>");
            continue;
        }
        let read = |e: &Option<TreeEntry>| -> UiResult<Vec<u8>> {
            match e {
                Some(e) => blob(store, repo, &e.id),
                None => Ok(Vec::new()),
            }
        };
        let (ob, nb) = (read(&d.old)?, read(&d.new)?);
        if is_binary(&ob) || is_binary(&nb) {
            if ob != nb {
                b.push_str("<p class=\"note\">Binary file differs.</p>");
            }
            b.push_str("</section>");
            continue;
        }
        b.push_str(&unified_html(
            &String::from_utf8_lossy(&ob),
            &String::from_utf8_lossy(&nb),
            &mut budget,
        ));
        b.push_str("</section>");
    }
    Ok(b)
}

fn unified_html(old: &str, new: &str, budget: &mut usize) -> String {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(old, new);
    let mut b = String::from(
        "<div class=\"scroll\"><table class=\"diff\"><thead class=\"sr-only\"><tr>\
         <th scope=\"col\">Old line</th><th scope=\"col\">New line</th>\
         <th scope=\"col\">Change</th></tr></thead><tbody>",
    );
    'hunks: for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        let _ = write!(
            b,
            "<tr class=\"hunk\"><td colspan=\"3\">{}</td></tr>",
            esc(&hunk.header().to_string())
        );
        for ch in hunk.iter_changes() {
            if *budget == 0 {
                b.push_str("<tr class=\"hunk\"><td colspan=\"3\">… diff truncated</td></tr>");
                break 'hunks;
            }
            *budget -= 1;
            let (cls, sign) = match ch.tag() {
                ChangeTag::Insert => ("add", '+'),
                ChangeTag::Delete => ("del", '-'),
                ChangeTag::Equal => ("ctx", ' '),
            };
            let num = |n: Option<usize>| n.map(|n| (n + 1).to_string()).unwrap_or_default();
            let text = ch.value();
            let _ = write!(
                b,
                "<tr class=\"{cls}\"><td class=\"ln\">{}</td><td class=\"ln\">{}</td>\
                 <td class=\"src\"><span class=\"sign\">{sign}</span>{}</td></tr>",
                num(ch.old_index()),
                num(ch.new_index()),
                esc(text.strip_suffix('\n').unwrap_or(text))
            );
        }
    }
    b.push_str("</tbody></table></div>");
    b
}

fn mode_word(m: Mode) -> &'static str {
    match m {
        Mode::File => "file",
        Mode::Exec => "executable",
        Mode::Symlink => "symlink",
        Mode::Tree => "directory",
        Mode::Gitlink => "submodule",
    }
}

// ---------------------------------------------------------------------------
// HTML helpers

/// `crumbs` labels are HTML; callers escape them.
fn layout(title: &str, crumbs: &[(String, Option<String>)], signed_in: bool, body: &str) -> String {
    let mut nav = String::from("<a href=\"/ui\">ghf</a>");
    for (label, href) in crumbs {
        match href {
            Some(h) => {
                let _ = write!(
                    nav,
                    " <span aria-hidden=\"true\">/</span> <a href=\"{h}\">{label}</a>"
                );
            }
            None => {
                let _ = write!(
                    nav,
                    " <span aria-hidden=\"true\">/</span> <span aria-current=\"page\">{label}</span>"
                );
            }
        }
    }
    let logout = if signed_in {
        "<form method=\"post\" action=\"/ui/logout\"><button type=\"submit\" class=\"link\">Sign out</button></form>"
    } else {
        ""
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{} · ghf</title><link rel=\"stylesheet\" href=\"/ui/style.css?v={version}\"></head>\
         <body><header><nav aria-label=\"Breadcrumb\">{nav}</nav>{logout}</header>\
         <main>{body}</main><footer class=\"muted small\">ghf-server {version} · read-only viewer</footer>\
         </body></html>",
        esc(title),
        version = env!("CARGO_PKG_VERSION")
    )
}

fn meta(b: &mut String, k: &str, v: &str) {
    let _ = write!(b, "<dt>{k}</dt><dd>{v}</dd>");
}

fn state_badge(t: &TraceRecord) -> String {
    let s = t.state.as_str();
    let extra = if t.state == TraceState::Finalized && t.abandoned {
        " (abandoned)"
    } else {
        ""
    };
    format!("<span class=\"badge state-{s}\">{s}{extra}</span>")
}

/// A trace's name, or its id in monospace.
fn trace_label(t: &TraceRecord) -> String {
    match &t.name {
        Some(n) => esc(n),
        None => format!("<code>{}</code>", esc(&t.id)),
    }
}

fn repo_base(repo: &str) -> String {
    format!("/ui/{}", enc_path(repo))
}

fn truncate(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn time_html(ms: i64) -> String {
    let t = UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    let iso = humantime::format_rfc3339_seconds(t).to_string();
    let shown = iso.replacen('T', " ", 1).replace('Z', " UTC");
    format!("<time datetime=\"{iso}\">{shown}</time>")
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    v /= 1024.0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

fn file_link(base: &str, ck: &str, path: &str) -> String {
    format!(
        "<a class=\"path\" href=\"{base}/checkpoints/{}/files/{}\">{}</a>",
        enc(ck),
        enc_path(path),
        esc(path)
    )
}

/// Escape text for HTML content and attribute values.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn percent(s: &str, keep_slash: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') {
            o.push(b as char);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

/// Percent-encode one URL component (query value or path segment).
fn enc(s: &str) -> String {
    percent(s, false)
}

/// Percent-encode a `/`-separated path, keeping the separators.
fn enc_path(s: &str) -> String {
    percent(s, true)
}

const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #ffffff; --fg: #1b1f24; --muted: #57606a; --border: #d0d7de;
  --panel: #f6f8fa; --link: #0550ae; --accent: #0969da;
  --add-bg: #dafbe1; --add-fg: #116329; --del-bg: #ffebe9; --del-fg: #a40e26;
  --hunk-bg: #ddf4ff; --note-bg: #fff8c5; --pass: #1a7f37; --fail: #cf222e;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117; --fg: #e6edf3; --muted: #9198a1; --border: #3d444d;
    --panel: #151b23; --link: #4493f8; --accent: #4493f8;
    --add-bg: #12261e; --add-fg: #6fdd8b; --del-bg: #2d1316; --del-fg: #ff9492;
    --hunk-bg: #121d2f; --note-bg: #272115; --pass: #3fb950; --fail: #f85149;
  }
}
* { box-sizing: border-box; }
html { font-size: 16px; }
body {
  margin: 0; background: var(--bg); color: var(--fg);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
header {
  display: flex; justify-content: space-between; align-items: center; gap: 1rem;
  padding: .75rem 1.25rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
header form { margin: 0; }
header a, header button.link { color: inherit; }
main { max-width: 1200px; margin: 0 auto; padding: 1.25rem; }
footer { max-width: 1200px; margin: 2rem auto; padding: 0 1.25rem; }
a { color: var(--link); }
a:focus-visible, button:focus-visible, select:focus-visible, input:focus-visible {
  outline: 2px solid var(--accent); outline-offset: 2px;
}
h1 { font-size: 1.5rem; margin: .25rem 0 1rem; overflow-wrap: anywhere; }
h2 { font-size: 1.15rem; margin: 1.75rem 0 .75rem; }
h3 { font-size: 1rem; margin: 0; }
code, .path, .src, .hunk td, select {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .875rem;
}
h1 code { font-size: .9em; }
.muted { color: var(--muted); }
.small { font-size: .85rem; }
.note { background: var(--note-bg); border: 1px solid var(--border); padding: .5rem .75rem; border-radius: 6px; }
.error { color: var(--fail); font-weight: 600; }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; width: 100%; }
table.list th, table.list td { text-align: left; padding: .5rem .6rem; border-bottom: 1px solid var(--border); vertical-align: top; }
table.list caption { text-align: left; color: var(--muted); padding-bottom: .4rem; }
table.list thead th { font-size: .85rem; color: var(--muted); }
.num { text-align: right !important; font-variant-numeric: tabular-nums; }
.badge {
  display: inline-block; padding: 0 .5rem; border-radius: 999px; font-size: .8rem;
  border: 1px solid var(--border); background: var(--panel); white-space: nowrap;
}
.state-open { border-color: var(--accent); color: var(--accent); }
.state-finalized { border-color: var(--pass); color: var(--pass); }
.state-pruned { color: var(--muted); }
.badge.pass { border-color: var(--pass); color: var(--pass); font-weight: 600; }
.badge.fail { border-color: var(--fail); color: var(--fail); font-weight: 600; }
.badge.pass::before { content: "✓ "; }
.badge.fail::before { content: "✗ "; }
dl.meta { display: grid; grid-template-columns: max-content 1fr; gap: .25rem 1rem; margin: 0 0 1rem; }
dl.meta dt { color: var(--muted); }
dl.meta dd { margin: 0; overflow-wrap: anywhere; }
ul.repos { padding-left: 1.25rem; }
ul.repos li { margin: .3rem 0; }
form.compare { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; margin: 1rem 0; }
select, input, button {
  font: inherit; color: var(--fg); background: var(--bg);
  border: 1px solid var(--border); border-radius: 6px; padding: .3rem .5rem;
}
select { max-width: 22rem; }
button { cursor: pointer; background: var(--panel); }
button.link { border: 0; background: none; color: var(--link); text-decoration: underline; padding: 0; }
form.login { display: flex; flex-direction: column; gap: .5rem; max-width: 28rem; }
ol.timeline { list-style: none; padding: 0; margin: 0; border-left: 2px solid var(--border); }
ol.timeline.single { border-left: 0; }
li.checkpoint { position: relative; margin: 0 0 1rem 1rem; padding: .75rem 1rem; border: 1px solid var(--border); border-radius: 8px; }
ol.timeline:not(.single) li.checkpoint::before {
  content: ""; position: absolute; left: calc(-1rem - 7px); top: 1.1rem;
  width: 12px; height: 12px; border-radius: 50%; background: var(--border);
}
li.checkpoint.pass { border-left: 4px solid var(--pass); }
li.checkpoint.fail { border-left: 4px solid var(--fail); }
ol.timeline:not(.single) li.checkpoint.pass::before { background: var(--pass); }
ol.timeline:not(.single) li.checkpoint.fail::before { background: var(--fail); }
.checkpoint .head { font-weight: 600; overflow-wrap: anywhere; }
.checkpoint .seq { margin-right: .25rem; }
ul.annotations { list-style: none; display: flex; flex-wrap: wrap; gap: .35rem; padding: 0; margin: .5rem 0 0; }
ul.changes { list-style: none; padding: 0; margin: .5rem 0 0; }
ul.changes li { overflow-wrap: anywhere; }
.st { display: inline-block; width: 1.3rem; text-align: center; font-family: ui-monospace, monospace; font-weight: 700; border-radius: 4px; }
.st.add { color: var(--add-fg); background: var(--add-bg); }
.st.del { color: var(--del-fg); background: var(--del-bg); }
.st.mod { color: var(--accent); background: var(--hunk-bg); }
.st.skip { color: var(--muted); background: var(--panel); }
.actions { margin-top: .5rem; }
section.file { border: 1px solid var(--border); border-radius: 8px; margin: 1rem 0; overflow: hidden; }
section.file h3 { background: var(--panel); padding: .5rem .75rem; border-bottom: 1px solid var(--border); overflow-wrap: anywhere; }
section.file > p { margin: .5rem .75rem; }
table.diff td, table.code td { padding: 0 .5rem; vertical-align: top; }
td.ln { width: 1%; min-width: 3rem; text-align: right; color: var(--muted); user-select: none; font-family: ui-monospace, monospace; font-size: .8rem; border-right: 1px solid var(--border); }
td.src { white-space: pre; }
.sign { display: inline-block; width: 1.1em; user-select: none; }
tr.add td.src { background: var(--add-bg); color: var(--add-fg); }
tr.del td.src { background: var(--del-bg); color: var(--del-fg); }
tr.add td.ln { background: var(--add-bg); }
tr.del td.ln { background: var(--del-bg); }
tr.hunk td { background: var(--hunk-bg); color: var(--muted); padding: .2rem .5rem; }
table.code { border: 1px solid var(--border); border-radius: 6px; }
.sr-only { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }
"#;
