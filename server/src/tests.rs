//! In-process tests for token scopes and the web UI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ghf_proto::id::{CHECKPOINT, TRACE, new_id, now_ms};
use ghf_proto::object::{self, Kind, Mode, TreeEntry};
use ghf_proto::{
    Change, ChangeStatus, CheckpointRecord, ObjectId, Policy, TraceRecord, TraceState,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::http::{AppState, router};
use crate::store::Store;

struct Fixture {
    app: Router,
    dir: PathBuf,
    rw: String,
    ro: String,
    other: String,
    trace: String,
    fork: String,
    ck: Vec<String>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, req: Request<Body>) -> Resp {
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    Resp {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn api(method: &str, path: &str, token: &str, body: Body, ctype: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/v1/repos/acme/app/{path}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, ctype)
        .body(body)
        .unwrap()
}

fn json_req(method: &str, path: &str, token: &str, v: &Value) -> Request<Body> {
    api(
        method,
        path,
        token,
        Body::from(v.to_string()),
        "application/json",
    )
}

fn page(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

fn obj(kind: Kind, body: &[u8]) -> (ObjectId, Vec<u8>) {
    let raw = object::raw(kind, body);
    (object::hash_raw(&raw), object::compress(&raw))
}

fn entry(name: &str, mode: Mode, id: ObjectId, size: u64) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        mode,
        id,
        size,
    }
}

fn checkpoint(
    trace: &str,
    seq: i64,
    parent: Option<&str>,
    root: ObjectId,
    message: &str,
    annotations: &[(&str, &str)],
    changes: &[(&str, ChangeStatus)],
) -> CheckpointRecord {
    CheckpointRecord {
        id: new_id(CHECKPOINT),
        trace_id: trace.into(),
        seq,
        parent: parent.map(Into::into),
        context_id: "wc_test".into(),
        base_rev: None,
        created_at: now_ms() + seq,
        author: "agent-7".into(),
        root,
        files: 3,
        bytes: 42,
        message: Some(message.into()),
        annotations: annotations
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
        changes: changes
            .iter()
            .map(|(p, s)| Change {
                path: p.to_string(),
                status: *s,
                from: None,
            })
            .collect(),
        skipped: vec![],
        index: None,
    }
}

fn trace_record(id: &str, forked_from: Option<&str>) -> TraceRecord {
    TraceRecord {
        id: id.into(),
        name: None,
        state: TraceState::Open,
        created_at: now_ms(),
        context_id: "wc_test".into(),
        forked_from: forked_from.map(Into::into),
        closed_at: None,
        finalized_at: None,
        final_commit: None,
        abandoned: false,
        pruned_at: None,
        holds: vec![],
    }
}

/// A server with one trace of three checkpoints in `acme/app`, plus a fork
/// of its first checkpoint, all pushed through the API with a writable token.
async fn fixture() -> Fixture {
    let dir =
        std::env::temp_dir().join(format!("ghf-server-test-{}", ghf_proto::id::random_hex(8)));
    let store = Store::open(&dir).unwrap();
    let rw = store.create_token("rw", "acme/*", false).unwrap();
    let ro = store.create_token("ro", "acme/*", true).unwrap();
    let other = store.create_token("other", "other/x", true).unwrap();
    let app = router(Arc::new(AppState {
        store: Mutex::new(store),
        server_id: "sv_test".into(),
        policy: Policy::default(),
        secure_cookies: false,
    }));

    // Objects for three manifests.
    let hello = obj(Kind::Blob, b"hello\n");
    let world = obj(Kind::Blob, b"hello\nworld <b>\n");
    let png = obj(Kind::Blob, b"\x89PNG\0\x01\x02");
    let png2 = obj(Kind::Blob, b"\x89PNG\0\x03\x04");
    let lib = obj(Kind::Blob, b"fn main() {}\n");
    let src = obj(
        Kind::Tree,
        &object::encode_tree(&[entry("lib.rs", Mode::File, lib.0, 13)]),
    );
    let root = |readme: &(ObjectId, Vec<u8>), size: u64, img: &(ObjectId, Vec<u8>)| {
        obj(
            Kind::Tree,
            &object::encode_tree(&[
                entry("README.md", Mode::File, readme.0, size),
                entry("logo.png", Mode::File, img.0, 8),
                entry("src", Mode::Tree, src.0, 0),
            ]),
        )
    };
    let r1 = root(&hello, 6, &png);
    let r2 = root(&world, 16, &png);
    let r3 = root(&world, 16, &png2);
    let mut batch = Vec::new();
    for (id, data) in [&hello, &world, &png, &png2, &lib, &src, &r1, &r2, &r3] {
        ghf_proto::api::frame(&mut batch, id, data);
    }
    let r = send(
        &app,
        api(
            "POST",
            "objects/batch",
            &rw,
            Body::from(batch),
            "application/octet-stream",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);

    let trace = new_id(TRACE);
    let r = send(
        &app,
        json_req(
            "PUT",
            &format!("traces/{trace}"),
            &rw,
            &json!(trace_record(&trace, None)),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let c1 = checkpoint(
        &trace,
        1,
        None,
        r1.0,
        "scaffold",
        &[],
        &[
            ("README.md", ChangeStatus::Added),
            ("logo.png", ChangeStatus::Added),
            ("src/lib.rs", ChangeStatus::Added),
        ],
    );
    let c2 = checkpoint(
        &trace,
        2,
        Some(&c1.id),
        r2.0,
        "add world",
        &[("tests", "pass")],
        &[("README.md", ChangeStatus::Modified)],
    );
    let c3 = checkpoint(
        &trace,
        3,
        Some(&c2.id),
        r3.0,
        "swap logo",
        &[("tests", "fail")],
        &[("logo.png", ChangeStatus::Modified)],
    );
    for c in [&c1, &c2, &c3] {
        let r = send(
            &app,
            json_req("PUT", &format!("checkpoints/{}", c.id), &rw, &json!(c)),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    }

    let fork = new_id(TRACE);
    let r = send(
        &app,
        json_req(
            "PUT",
            &format!("traces/{fork}"),
            &rw,
            &json!(trace_record(&fork, Some(&c1.id))),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    Fixture {
        app,
        dir,
        rw,
        ro,
        other,
        trace,
        fork,
        ck: vec![c1.id, c2.id, c3.id],
    }
}

async fn sign_in(f: &Fixture, token: &str) -> String {
    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={token}&next=%2Fui%2Facme%2Fapp")))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.headers[header::LOCATION], "/ui/acme/app");
    let cookie = r.headers[header::SET_COOKIE].to_str().unwrap().to_owned();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/ui"), "{cookie}");
    cookie.split(';').next().unwrap().to_owned()
}

#[tokio::test]
async fn read_only_tokens_are_refused_on_every_mutating_route() {
    let f = fixture().await;
    let (ck, tr) = (&f.ck[0], &f.trace);
    let (oid, data) = obj(Kind::Blob, b"new\n");
    let mut batch = Vec::new();
    ghf_proto::api::frame(&mut batch, &oid, &data);
    let fresh = checkpoint(tr, 4, Some(&f.ck[2]), obj(Kind::Tree, b"").0, "x", &[], &[]);
    let octet = "application/octet-stream";
    let cases: Vec<(&str, Request<Body>)> = vec![
        (
            "objects/missing",
            json_req("POST", "objects/missing", &f.ro, &json!({"ids": [oid]})),
        ),
        (
            "objects/{id}",
            api(
                "PUT",
                &format!("objects/{oid}"),
                &f.ro,
                Body::from(data.clone()),
                octet,
            ),
        ),
        (
            "objects/batch",
            api("POST", "objects/batch", &f.ro, Body::from(batch), octet),
        ),
        (
            "traces/{id}",
            json_req(
                "PUT",
                &format!("traces/{tr}"),
                &f.ro,
                &json!(trace_record(tr, None)),
            ),
        ),
        (
            "checkpoints/{id}",
            json_req(
                "PUT",
                &format!("checkpoints/{}", fresh.id),
                &f.ro,
                &json!(fresh),
            ),
        ),
        (
            "holds PUT",
            json_req("PUT", &format!("traces/{tr}/holds/pin"), &f.ro, &json!({})),
        ),
        (
            "holds DELETE",
            api(
                "DELETE",
                &format!("traces/{tr}/holds/pin"),
                &f.ro,
                Body::empty(),
                octet,
            ),
        ),
        (
            "pending",
            json_req("PUT", "clients/c1/pending", &f.ro, &json!({"traces": [tr]})),
        ),
        (
            "prune",
            json_req("POST", "prune", &f.ro, &json!({"dry_run": true})),
        ),
    ];
    for (what, req) in cases {
        let r = send(&f.app, req).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{what}: {}", r.body);
        assert_eq!(r.json()["code"], "E_AUTH", "{what}: {}", r.body);
        assert!(r.body.contains("read-only"), "{what}: {}", r.body);
    }

    // Nothing changed: the object was not stored and no hold was added.
    let r = send(
        &f.app,
        api(
            "GET",
            &format!("objects/{oid}"),
            &f.rw,
            Body::empty(),
            octet,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = send(
        &f.app,
        api("GET", &format!("traces/{tr}"), &f.ro, Body::empty(), octet),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["holds"], json!([]));

    // Reads work with the read-only token.
    let reads = vec![
        api("GET", &format!("traces/{tr}"), &f.ro, Body::empty(), octet),
        api("GET", "changes?since=0", &f.ro, Body::empty(), octet),
        api("GET", "policy", &f.ro, Body::empty(), octet),
        json_req("POST", "objects/fetch", &f.ro, &json!({"ids": [oid]})),
    ];
    for req in reads {
        let r = send(&f.app, req).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }
    let r = send(
        &f.app,
        api("GET", "changes?since=0", &f.ro, Body::empty(), octet),
    )
    .await;
    assert_eq!(r.json()["checkpoints"].as_array().unwrap().len(), 3);
    assert!(r.body.contains(ck));

    // The same write succeeds with a read-write token.
    let r = send(
        &f.app,
        json_req("PUT", &format!("traces/{tr}/holds/pin"), &f.rw, &json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

#[tokio::test]
async fn ui_requires_sign_in_and_rejects_bad_tokens() {
    let f = fixture().await;
    let r = send(&f.app, page("/ui/acme/app", None)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        r.headers[header::LOCATION],
        "/ui/login?next=%2Fui%2Facme%2Fapp"
    );

    // A cross-site navigation without the Strict cookie reloads once.
    let r = send(
        &f.app,
        Request::builder()
            .uri("/ui/acme/app")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("http-equiv=\"refresh\""), "{}", r.body);

    let r = send(&f.app, page("/ui/login?next=/ui/acme/app", None)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("<form method=\"post\" action=\"/ui/login\"")
    );
    assert!(
        r.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );

    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("token=ghf_nope"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers.get(header::SET_COOKIE).is_none());

    // Cross-origin sign-in is refused.
    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header(header::HOST, "ghf.example")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={}", f.ro)))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header("sec-fetch-site", "cross-site")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={}", f.ro)))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // A browser's same-origin post is accepted.
    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header(header::HOST, "ghf.example")
            .header(header::ORIGIN, "https://ghf.example")
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={}", f.ro)))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    // Off-site `next` targets fall back to /ui.
    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "token={}&next=%2F%2Fevil.example",
                f.ro
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(r.headers[header::LOCATION], "/ui");

    // Signing out clears the cookie.
    let cookie = sign_in(&f, &f.rw).await;
    let r = send(
        &f.app,
        Request::builder()
            .method("POST")
            .uri("/ui/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        r.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
}

#[tokio::test]
async fn ui_pages_render_a_seeded_trace() {
    let f = fixture().await;
    let cookie = sign_in(&f, &f.ro).await;
    let get = |p: String| send(&f.app, page(&p, Some(&cookie)));

    let r = get("/ui".into()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("href=\"/ui/acme/app\""), "{}", r.body);

    let r = get("/ui/style.css".into()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("prefers-color-scheme: dark"));

    // Trace list.
    let r = get("/ui/acme/app".into()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains(&format!("/ui/acme/app/traces/{}", f.trace)));
    assert!(r.body.contains("agent-7"));
    assert!(r.body.contains("state-open"));
    assert!(r.body.contains("<td class=\"num\">3</td>"), "{}", r.body);
    assert!(r.body.contains("forked from"));

    // Timeline.
    let r = get(format!("/ui/acme/app/traces/{}", f.trace)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    for msg in ["scaffold", "add world", "swap logo"] {
        assert!(r.body.contains(msg), "{msg}");
    }
    assert!(r.body.contains("class=\"checkpoint pass\""));
    assert!(r.body.contains("class=\"checkpoint fail\""));
    assert!(r.body.contains("<li class=\"badge pass\">tests=pass</li>"));
    assert!(r.body.contains("<li class=\"badge fail\">tests=fail</li>"));
    assert!(r.body.contains(&format!(
        "/ui/acme/app/checkpoints/{}/files/src/lib.rs",
        f.ck[0]
    )));
    assert!(
        r.body.contains(&format!("/ui/acme/app/traces/{}", f.fork)),
        "fork listed"
    );
    assert!(r.body.contains("action=\"/ui/acme/app/diff\""));

    // The fork links to its base.
    let r = get(format!("/ui/acme/app/traces/{}", f.fork)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Forked from"));
    assert!(
        r.body
            .contains(&format!("/ui/acme/app/checkpoints/{}", f.ck[0]))
    );
    assert!(r.body.contains(&format!("/ui/acme/app/traces/{}", f.trace)));

    // Checkpoint vs parent: escaped text diff.
    let r = get(format!("/ui/acme/app/checkpoints/{}", f.ck[1])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(
        r.body
            .contains("<span class=\"sign\">+</span>world &lt;b&gt;"),
        "{}",
        r.body
    );
    assert!(!r.body.contains("world <b>"));
    assert!(r.body.contains("README.md"));
    assert!(
        !r.body.contains("lib.rs</a></li>"),
        "unchanged subtree skipped"
    );

    // First checkpoint diffs against an empty tree.
    let r = get(format!("/ui/acme/app/checkpoints/{}", f.ck[0])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("empty tree"));
    assert!(r.body.contains("src/lib.rs"));

    // Binary change.
    let r = get(format!("/ui/acme/app/checkpoints/{}", f.ck[2])).await;
    assert!(r.body.contains("Binary file differs"), "{}", r.body);

    // Arbitrary pair.
    let r = get(format!("/ui/acme/app/diff?from={}&to={}", f.ck[0], f.ck[2])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("2 file(s) changed"), "{}", r.body);
    assert!(r.body.contains("+</span>world"));
    assert!(r.body.contains("Binary file differs"));

    // Files at a checkpoint.
    let r = get(format!("/ui/acme/app/checkpoints/{}/files", f.ck[1])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains(">src/</a>"));
    let r = get(format!(
        "/ui/acme/app/checkpoints/{}/files/README.md",
        f.ck[1]
    ))
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("world &lt;b&gt;"));
    let r = get(format!(
        "/ui/acme/app/checkpoints/{}/files/src/lib.rs",
        f.ck[0]
    ))
    .await;
    assert!(r.body.contains("fn main() {}"), "{}", r.body);
    let r = get(format!(
        "/ui/acme/app/checkpoints/{}/files/logo.png",
        f.ck[0]
    ))
    .await;
    assert!(r.body.contains("Binary file, not shown"));
    let r = get(format!(
        "/ui/acme/app/checkpoints/{}/files/nope.txt",
        f.ck[0]
    ))
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // No page offers a write action: the only forms are sign-out and the
    // GET compare form.
    let r = get(format!("/ui/acme/app/traces/{}", f.trace)).await;
    let posts = r.body.matches("method=\"post\"").count();
    assert_eq!(posts, 1, "only sign-out posts");
    assert!(r.body.contains("action=\"/ui/logout\""));
}

#[tokio::test]
async fn ui_follows_token_repo_scope() {
    let f = fixture().await;
    let cookie = sign_in(&f, &f.other).await;
    let get = |p: String| send(&f.app, page(&p, Some(&cookie)));

    let r = get("/ui".into()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("acme/app"), "{}", r.body);

    for p in [
        "/ui/acme/app".to_owned(),
        format!("/ui/acme/app/traces/{}", f.trace),
        format!("/ui/acme/app/checkpoints/{}", f.ck[1]),
        format!("/ui/acme/app/checkpoints/{}/files/README.md", f.ck[1]),
        format!("/ui/acme/app/diff?from={}&to={}", f.ck[0], f.ck[1]),
    ] {
        let r = get(p.clone()).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}");
        assert!(!r.body.contains("scaffold"), "{p}");
    }

    // The API applies the same scope to read-only tokens.
    let r = send(
        &f.app,
        api(
            "GET",
            &format!("traces/{}", f.trace),
            &f.other,
            Body::empty(),
            "application/json",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["code"], "E_AUTH");

    // A checkpoint id from another repository is not found.
    let r = send(
        &f.app,
        page(
            &format!("/ui/other/x/checkpoints/{}", f.ck[0]),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[test]
fn old_databases_gain_the_read_only_column() {
    let dir =
        std::env::temp_dir().join(format!("ghf-server-test-{}", ghf_proto::id::random_hex(8)));
    std::fs::create_dir_all(&dir).unwrap();
    {
        let db = rusqlite::Connection::open(dir.join("server.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE tokens (hash TEXT PRIMARY KEY, name TEXT NOT NULL, repos TEXT NOT NULL,
             created_at INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
             INSERT INTO tokens VALUES ('h', 'old', 'acme/*', 0, 0);",
        )
        .unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let tokens = store.list_tokens().unwrap();
    assert_eq!(tokens.len(), 1);
    assert!(!tokens[0].read_only, "existing tokens stay read-write");
    let t = store.create_token("ro", "acme/app", true).unwrap();
    assert!(store.token(&t).unwrap().unwrap().read_only);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
