//! Server storage: one SQLite database holding metadata and per-repository
//! content-addressed objects.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ghf_proto::object::{self, Mode};
use ghf_proto::{CheckpointRecord, ObjectId, Policy, TraceRecord, TraceState};
use rusqlite::{Connection, OptionalExtension, params};

pub type DbResult<T> = Result<T, Error>;

#[derive(Debug)]
pub struct Error {
    pub code: ghf_proto::ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ghf_proto::ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(ghf_proto::ErrorCode::Internal, format!("database: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ghf_proto::ErrorCode::Internal, format!("I/O: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ghf_proto::ErrorCode::Internal, format!("JSON: {e}"))
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tokens (
    hash TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    repos TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    read_only INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS repos (
    name TEXT PRIMARY KEY,
    after_finalize_ms INTEGER,
    unfinalized_cap_ms INTEGER
);
CREATE TABLE IF NOT EXISTS traces (
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    record TEXT NOT NULL,
    state TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE INDEX IF NOT EXISTS traces_seq ON traces(repo, seq);
CREATE TABLE IF NOT EXISTS checkpoints (
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    record TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE INDEX IF NOT EXISTS checkpoints_seq ON checkpoints(repo, seq);
CREATE INDEX IF NOT EXISTS checkpoints_trace ON checkpoints(repo, trace_id);
CREATE TABLE IF NOT EXISTS pending (
    repo TEXT NOT NULL,
    client TEXT NOT NULL,
    traces TEXT NOT NULL,
    reported_at INTEGER NOT NULL,
    PRIMARY KEY (repo, client)
);
CREATE TABLE IF NOT EXISTS objects (
    repo TEXT NOT NULL,
    id BLOB NOT NULL,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE TABLE IF NOT EXISTS counter (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL
);
INSERT OR IGNORE INTO counter (id, seq) VALUES (1, 0);
"#;

pub struct Store {
    pub db: Connection,
    pub dir: PathBuf,
}

/// A valid token's grants.
#[derive(Debug, Clone)]
pub struct TokenInfo {
    /// Comma-separated scopes: `owner/name`, `owner/*` or `*`.
    pub repos: String,
    pub read_only: bool,
}

impl TokenInfo {
    /// Whether the token's scope covers `owner/name`.
    pub fn allows(&self, owner: &str, name: &str) -> bool {
        let repo = format!("{owner}/{name}");
        self.repos
            .split(',')
            .map(str::trim)
            .any(|r| r == "*" || r == repo || r.strip_suffix("/*").is_some_and(|o| o == owner))
    }
}

/// A token as `token list` shows it.
pub struct TokenRow {
    pub name: String,
    pub repos: String,
    pub created_at: i64,
    pub revoked: bool,
    pub read_only: bool,
}

/// Per-trace summary for listings.
#[derive(Debug, Clone, Default)]
pub struct TraceStats {
    pub checkpoints: u64,
    pub last_checkpoint_at: Option<i64>,
    /// Distinct checkpoint authors, sorted.
    pub authors: Vec<String>,
}

pub fn hash_token(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

impl Store {
    pub fn open(dir: &Path) -> DbResult<Self> {
        fs::create_dir_all(dir)?;
        let db = Connection::open(dir.join("server.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(SCHEMA)?;
        // Databases created before read-only tokens lack the column.
        let has_read_only: bool = db.query_row(
            "SELECT count(*) FROM pragma_table_info('tokens') WHERE name = 'read_only'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_read_only {
            db.execute_batch("ALTER TABLE tokens ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0")?;
        }
        Ok(Self {
            db,
            dir: dir.to_path_buf(),
        })
    }

    /// Stable id for this server instance, created on first start.
    pub fn server_id(&self) -> DbResult<String> {
        let path = self.dir.join("server-id");
        if let Ok(s) = fs::read_to_string(&path) {
            return Ok(s.trim().to_owned());
        }
        let id = ghf_proto::id::new_id(ghf_proto::id::SERVER);
        fs::write(&path, format!("{id}\n"))?;
        Ok(id)
    }

    fn next_seq(&self) -> DbResult<i64> {
        Ok(self.db.query_row(
            "UPDATE counter SET seq = seq + 1 WHERE id = 1 RETURNING seq",
            [],
            |r| r.get(0),
        )?)
    }

    // -- tokens -----------------------------------------------------------------

    pub fn create_token(&self, name: &str, repos: &str, read_only: bool) -> DbResult<String> {
        let token = format!("ghf_{}", ghf_proto::id::random_hex(32));
        self.db.execute(
            "INSERT INTO tokens (hash, name, repos, created_at, read_only) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![hash_token(&token), name, repos, ghf_proto::id::now_ms(), read_only],
        )?;
        Ok(token)
    }

    /// A token's grants, if it is valid.
    pub fn token(&self, token: &str) -> DbResult<Option<TokenInfo>> {
        Ok(self
            .db
            .query_row(
                "SELECT repos, read_only FROM tokens WHERE hash = ?1 AND revoked = 0",
                [hash_token(token)],
                |r| {
                    Ok(TokenInfo {
                        repos: r.get(0)?,
                        read_only: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_tokens(&self) -> DbResult<Vec<TokenRow>> {
        let mut st = self.db.prepare(
            "SELECT name, repos, created_at, revoked, read_only FROM tokens ORDER BY created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok(TokenRow {
                name: r.get(0)?,
                repos: r.get(1)?,
                created_at: r.get(2)?,
                revoked: r.get::<_, i64>(3)? != 0,
                read_only: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke_token(&self, name: &str) -> DbResult<usize> {
        Ok(self
            .db
            .execute("UPDATE tokens SET revoked = 1 WHERE name = ?1", [name])?)
    }

    // -- policy -----------------------------------------------------------------

    pub fn policy(&self, repo: &str, default: &Policy) -> DbResult<Policy> {
        let row: Option<(Option<i64>, Option<i64>)> = self
            .db
            .query_row(
                "SELECT after_finalize_ms, unfinalized_cap_ms FROM repos WHERE name = ?1",
                [repo],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut p = *default;
        if let Some((a, u)) = row {
            if let Some(a) = a {
                p.after_finalize_ms = a;
            }
            if let Some(u) = u {
                p.unfinalized_cap_ms = u;
            }
        }
        Ok(p)
    }

    pub fn set_policy(&self, repo: &str, after: Option<i64>, cap: Option<i64>) -> DbResult<()> {
        self.db.execute(
            "INSERT INTO repos (name, after_finalize_ms, unfinalized_cap_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET
               after_finalize_ms = COALESCE(?2, after_finalize_ms),
               unfinalized_cap_ms = COALESCE(?3, unfinalized_cap_ms)",
            params![repo, after, cap],
        )?;
        Ok(())
    }

    pub fn repos(&self) -> DbResult<Vec<String>> {
        let mut st = self.db.prepare("SELECT DISTINCT repo FROM traces")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // -- objects ----------------------------------------------------------------

    pub fn has_object(&self, repo: &str, id: &ObjectId) -> bool {
        self.db
            .query_row(
                "SELECT 1 FROM objects WHERE repo = ?1 AND id = ?2",
                params![repo, &id.0[..]],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn read_object(&self, repo: &str, id: &ObjectId) -> DbResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT data FROM objects WHERE repo = ?1 AND id = ?2",
                params![repo, &id.0[..]],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Verify and store objects in one transaction.
    pub fn write_objects(&self, repo: &str, objects: &[(ObjectId, &[u8])]) -> DbResult<()> {
        for (id, data) in objects {
            if object::verify(id, data).is_none() {
                return Err(Error::new(
                    ghf_proto::ErrorCode::Usage,
                    format!("object {} does not match its content", id.short()),
                ));
            }
        }
        let tx = self.db.unchecked_transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO objects (repo, id, data, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo, id) DO UPDATE SET created_at = excluded.created_at",
            )?;
            let now = ghf_proto::id::now_ms();
            for (id, data) in objects {
                st.execute(params![repo, &id.0[..], data, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Refresh objects' age so GC's grace period protects them.
    pub fn touch_objects(&self, repo: &str, ids: &[ObjectId]) -> DbResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        {
            let mut st =
                tx.prepare("UPDATE objects SET created_at = ?3 WHERE repo = ?1 AND id = ?2")?;
            let now = ghf_proto::id::now_ms();
            for id in ids {
                st.execute(params![repo, &id.0[..], now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read a tree object.
    pub fn tree(&self, repo: &str, id: &ObjectId) -> DbResult<Option<Vec<object::TreeEntry>>> {
        let Some(c) = self.read_object(repo, id)? else {
            return Ok(None);
        };
        let raw = object::decompress(&c)?;
        let body = match object::parse_raw(&raw) {
            Some((object::Kind::Tree, b)) => b,
            _ => {
                return Err(Error::new(
                    ghf_proto::ErrorCode::Usage,
                    format!("object {} is not a tree", id.short()),
                ));
            }
        };
        object::decode_tree(body).map(Some).ok_or_else(|| {
            Error::new(
                ghf_proto::ErrorCode::Usage,
                format!("tree {} is malformed or has unsafe entry names", id.short()),
            )
        })
    }

    /// First missing object in the closure of `root`, if any.
    pub fn missing_in_closure(&self, repo: &str, root: &ObjectId) -> DbResult<Option<ObjectId>> {
        let mut stack = vec![*root];
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(entries) = self.tree(repo, &id)? else {
                return Ok(Some(id));
            };
            for e in entries {
                if e.mode == Mode::Tree {
                    stack.push(e.id);
                } else if !self.has_object(repo, &e.id) {
                    return Ok(Some(e.id));
                }
            }
        }
        Ok(None)
    }

    /// Mark every object reachable from `root`.
    pub fn mark(&self, repo: &str, root: &ObjectId, live: &mut HashSet<ObjectId>) -> DbResult<()> {
        let mut stack = vec![*root];
        while let Some(id) = stack.pop() {
            if !live.insert(id) {
                continue;
            }
            if let Some(entries) = self.tree(repo, &id)? {
                for e in entries {
                    if e.mode == Mode::Tree {
                        stack.push(e.id);
                    } else {
                        live.insert(e.id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Delete unreferenced objects older than `grace`. Returns (count, bytes).
    pub fn sweep(
        &self,
        repo: &str,
        live: &HashSet<ObjectId>,
        grace: std::time::Duration,
        dry_run: bool,
    ) -> DbResult<(u64, u64)> {
        let cutoff = ghf_proto::id::now_ms() - grace.as_millis() as i64;
        let candidates: Vec<(Vec<u8>, i64)> = {
            let mut st = self.db.prepare(
                "SELECT id, length(data) FROM objects WHERE repo = ?1 AND created_at <= ?2",
            )?;
            let rows = st.query_map(params![repo, cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let (mut n, mut bytes) = (0, 0);
        let tx = self.db.unchecked_transaction()?;
        for (id, len) in candidates {
            let Ok(arr) = <[u8; 32]>::try_from(id.as_slice()) else {
                continue;
            };
            if live.contains(&ObjectId(arr)) {
                continue;
            }
            if !dry_run {
                tx.execute(
                    "DELETE FROM objects WHERE repo = ?1 AND id = ?2",
                    params![repo, id],
                )?;
            }
            n += 1;
            bytes += len as u64;
        }
        tx.commit()?;
        Ok((n, bytes))
    }

    // -- traces -----------------------------------------------------------------

    pub fn trace(&self, repo: &str, id: &str) -> DbResult<Option<TraceRecord>> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT record FROM traces WHERE repo = ?1 AND id = ?2",
                params![repo, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    pub fn traces(&self, repo: &str) -> DbResult<Vec<TraceRecord>> {
        let mut st = self
            .db
            .prepare("SELECT record FROM traces WHERE repo = ?1 ORDER BY seq")?;
        let rows = st.query_map([repo], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn save_trace(&self, repo: &str, t: &TraceRecord) -> DbResult<()> {
        let seq = self.next_seq()?;
        self.db.execute(
            "INSERT INTO traces (repo, id, record, state, seq) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repo, id) DO UPDATE SET record = ?3, state = ?4, seq = ?5",
            params![repo, t.id, serde_json::to_string(t)?, t.state.as_str(), seq],
        )?;
        Ok(())
    }

    /// Merge a client's view of a trace. States only move forward; holds are
    /// managed through their own endpoints and kept from the server copy.
    pub fn merge_trace(&self, repo: &str, incoming: &TraceRecord) -> DbResult<TraceRecord> {
        let merged = match self.trace(repo, &incoming.id)? {
            None => {
                let mut t = incoming.clone();
                t.holds.clear();
                if t.state == TraceState::Pruned {
                    t.state = TraceState::Closed;
                }
                t
            }
            Some(mut t) => {
                let state = t.state.max(incoming.state);
                if state == t.state && t == *incoming {
                    return Ok(t);
                }
                t.name = t.name.or_else(|| incoming.name.clone());
                if t.state != TraceState::Pruned && incoming.state != TraceState::Pruned {
                    t.state = state;
                }
                t.closed_at = t.closed_at.or(incoming.closed_at);
                if t.finalized_at.is_none() && incoming.finalized_at.is_some() {
                    t.finalized_at = incoming.finalized_at;
                    t.final_commit = incoming.final_commit.clone();
                    t.abandoned = incoming.abandoned;
                }
                t
            }
        };
        self.save_trace(repo, &merged)?;
        Ok(merged)
    }

    // -- checkpoints ------------------------------------------------------------

    pub fn has_checkpoint(&self, repo: &str, id: &str) -> DbResult<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT 1 FROM checkpoints WHERE repo = ?1 AND id = ?2",
                params![repo, id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn insert_checkpoint(&self, repo: &str, c: &CheckpointRecord) -> DbResult<()> {
        let seq = self.next_seq()?;
        self.db.execute(
            "INSERT OR IGNORE INTO checkpoints (repo, id, trace_id, record, seq) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo, c.id, c.trace_id, serde_json::to_string(c)?, seq],
        )?;
        Ok(())
    }

    pub fn checkpoints(&self, repo: &str) -> DbResult<Vec<CheckpointRecord>> {
        let mut st = self
            .db
            .prepare("SELECT record FROM checkpoints WHERE repo = ?1")?;
        let rows = st.query_map([repo], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn checkpoint(&self, repo: &str, id: &str) -> DbResult<Option<CheckpointRecord>> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT record FROM checkpoints WHERE repo = ?1 AND id = ?2",
                params![repo, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    /// A trace's checkpoints in trace order.
    pub fn trace_checkpoints(&self, repo: &str, trace_id: &str) -> DbResult<Vec<CheckpointRecord>> {
        let mut st = self
            .db
            .prepare("SELECT record FROM checkpoints WHERE repo = ?1 AND trace_id = ?2")?;
        let rows = st.query_map(params![repo, trace_id], |r| r.get::<_, String>(0))?;
        let mut out: Vec<CheckpointRecord> = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        out.sort_by_key(|c| c.seq);
        Ok(out)
    }

    /// Checkpoint count, latest checkpoint time and authors for every trace.
    pub fn trace_stats(
        &self,
        repo: &str,
    ) -> DbResult<std::collections::HashMap<String, TraceStats>> {
        let mut st = self.db.prepare(
            "SELECT trace_id, count(*), max(json_extract(record, '$.created_at')),
                    json_group_array(DISTINCT json_extract(record, '$.author'))
             FROM checkpoints WHERE repo = ?1 GROUP BY trace_id",
        )?;
        let rows = st.query_map([repo], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (id, n, last, authors) = r?;
            let mut authors: Vec<String> = serde_json::from_str(&authors)?;
            authors.sort();
            out.insert(
                id,
                TraceStats {
                    checkpoints: n as u64,
                    last_checkpoint_at: last,
                    authors,
                },
            );
        }
        Ok(out)
    }

    pub fn delete_checkpoints(
        &self,
        repo: &str,
        trace_id: &str,
        keep: &[String],
    ) -> DbResult<usize> {
        let mut st = self
            .db
            .prepare("SELECT id FROM checkpoints WHERE repo = ?1 AND trace_id = ?2")?;
        let ids: Vec<String> = st
            .query_map(params![repo, trace_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut n = 0;
        for id in ids.iter().filter(|i| !keep.contains(i)) {
            n += self.db.execute(
                "DELETE FROM checkpoints WHERE repo = ?1 AND id = ?2",
                params![repo, id],
            )?;
        }
        Ok(n)
    }

    /// Changes after `since`, at most `limit` of each kind.
    pub fn changes(
        &self,
        repo: &str,
        since: i64,
        limit: usize,
    ) -> DbResult<ghf_proto::api::Changes> {
        let fetch = |table: &str| -> DbResult<Vec<(i64, String)>> {
            let mut st = self.db.prepare(&format!(
                "SELECT seq, record FROM {table} WHERE repo = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
            ))?;
            let rows = st.query_map(params![repo, since, limit as i64 + 1], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        let mut t = fetch("traces")?;
        let mut c = fetch("checkpoints")?;
        let more = t.len() > limit || c.len() > limit;
        t.truncate(limit);
        c.truncate(limit);
        let next = if more {
            // Stop at the smaller frontier so nothing is skipped.
            let lt = t.last().map_or(i64::MAX, |x| x.0);
            let lc = c.last().map_or(i64::MAX, |x| x.0);
            let n = lt.min(lc);
            t.retain(|x| x.0 <= n);
            c.retain(|x| x.0 <= n);
            n
        } else {
            t.iter().chain(c.iter()).map(|x| x.0).max().unwrap_or(since)
        };
        let mut traces = Vec::new();
        for (_, j) in t {
            traces.push(serde_json::from_str(&j)?);
        }
        let mut checkpoints = Vec::new();
        for (_, j) in c {
            checkpoints.push(serde_json::from_str(&j)?);
        }
        Ok(ghf_proto::api::Changes {
            traces,
            checkpoints,
            next,
            more,
        })
    }

    // -- pending ----------------------------------------------------------------

    pub fn set_pending(&self, repo: &str, client: &str, traces: &[String]) -> DbResult<()> {
        if traces.is_empty() {
            self.db.execute(
                "DELETE FROM pending WHERE repo = ?1 AND client = ?2",
                params![repo, client],
            )?;
        } else {
            self.db.execute(
                "INSERT OR REPLACE INTO pending (repo, client, traces, reported_at) VALUES (?1, ?2, ?3, ?4)",
                params![repo, client, serde_json::to_string(traces)?, ghf_proto::id::now_ms()],
            )?;
        }
        Ok(())
    }

    pub fn pending_traces(&self, repo: &str) -> DbResult<HashSet<String>> {
        let mut st = self
            .db
            .prepare("SELECT traces FROM pending WHERE repo = ?1")?;
        let rows = st.query_map([repo], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for r in rows {
            let v: Vec<String> = serde_json::from_str(&r?)?;
            out.extend(v);
        }
        Ok(out)
    }
}
