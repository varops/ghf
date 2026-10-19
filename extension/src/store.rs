//! Local durable buffer: one SQLite database holding metadata and
//! content-addressed objects, shared by all worktrees of a repository under
//! `<git-common-dir>/ghf/`. Nothing here is a Git ref or Git object.
//!
//! A checkpoint's new objects and its record commit in one transaction, so a
//! crash leaves either the whole checkpoint or none of it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ghf_proto::object::{self, Kind, TreeEntry};
use ghf_proto::{CheckpointRecord, ObjectId, TraceRecord};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Context, Result};
use crate::{err, fail};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS traces (
    id TEXT PRIMARY KEY,
    record TEXT NOT NULL,
    state TEXT NOT NULL,
    context_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    writer TEXT,
    lease_at INTEGER,
    dirty INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS traces_context ON traces(context_id, state);
CREATE TABLE IF NOT EXISTS checkpoints (
    id TEXT PRIMARY KEY,
    trace_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    record TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    synced INTEGER NOT NULL DEFAULT 0,
    UNIQUE(trace_id, seq)
);
CREATE INDEX IF NOT EXISTS checkpoints_unsynced ON checkpoints(synced, created_at);
CREATE TABLE IF NOT EXISTS hold_ops (
    id INTEGER PRIMARY KEY,
    trace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    place INTEGER NOT NULL,
    expires_at INTEGER
);
CREATE TABLE IF NOT EXISTS failures (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    context_id TEXT,
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    strict INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS statcache (
    context_id TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    mode TEXT NOT NULL,
    id TEXT NOT NULL,
    PRIMARY KEY (context_id, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS idempotency (
    key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    response TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS objects (
    id BLOB PRIMARY KEY,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS remote_objects (
    id BLOB PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Store {
    pub db: Connection,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TraceRow {
    pub record: TraceRecord,
    pub writer: Option<String>,
    pub lease_at: Option<i64>,
}

/// A pending hold operation: (op id, trace, name, place, expires_at).
pub type HoldOp = (i64, String, String, bool, Option<i64>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatEntry {
    pub size: u64,
    pub mtime: i64,
    pub ino: u64,
    pub mode: object::Mode,
    pub id: ObjectId,
}

fn mode_str(m: object::Mode) -> &'static str {
    match m {
        object::Mode::File => "file",
        object::Mode::Exec => "exec",
        object::Mode::Symlink => "symlink",
        object::Mode::Tree => "tree",
        object::Mode::Gitlink => "gitlink",
    }
}

fn parse_mode(s: &str) -> object::Mode {
    match s {
        "exec" => object::Mode::Exec,
        "symlink" => object::Mode::Symlink,
        "tree" => object::Mode::Tree,
        "gitlink" => object::Mode::Gitlink,
        _ => object::Mode::File,
    }
}

impl Store {
    pub fn open(common_dir: &Path) -> Result<Self> {
        let dir = common_dir.join("ghf");
        fs::create_dir_all(dir.join("tmp")).context(|| format!("create {}", dir.display()))?;
        let db = Connection::open(dir.join("ghf.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        // WAL checkpoints are moved off the capture path; see `maintain`.
        db.pragma_update(None, "wal_autocheckpoint", 0)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db, dir })
    }

    /// Fold the write-ahead log back into the database once it grows past
    /// `threshold` bytes. Captures pass a high threshold so they rarely pay
    /// for this; other commands and background sync pass a low one.
    pub fn maintain(&self, threshold: u64) {
        let wal = fs::metadata(self.dir.join("ghf.db-wal")).map_or(0, |m| m.len());
        if wal > threshold {
            let _ = self
                .db
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        }
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.dir.join("tmp")
    }

    /// Run `f` in one write transaction (re-entrant: nested calls join the
    /// outer transaction). Commits on `Ok`, rolls back on `Err`.
    pub fn tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.db.is_autocommit() {
            return f();
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.db.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // -- objects ------------------------------------------------------------

    pub fn has_object(&self, id: &ObjectId) -> bool {
        self.db
            .query_row("SELECT 1 FROM objects WHERE id = ?1", [&id.0[..]], |_| {
                Ok(())
            })
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Store an object; returns its id.
    pub fn put(&self, kind: Kind, body: &[u8]) -> Result<ObjectId> {
        let id = object::hash(kind, body);
        if !self.has_object(&id) {
            let raw = object::raw(kind, body);
            self.write_object(&id, &object::compress(&raw))?;
        }
        Ok(id)
    }

    /// Store an already-compressed object after verifying it.
    pub fn put_compressed(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        if self.has_object(id) {
            return Ok(());
        }
        if object::verify(id, data).is_none() {
            fail!(Server, "object {} failed verification", id.short());
        }
        self.write_object(id, data)
    }

    fn write_object(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO objects (id, data, created_at) VALUES (?1, ?2, ?3)",
            params![&id.0[..], data, ghf_proto::id::now_ms()],
        )?;
        Ok(())
    }

    pub fn get_compressed(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row("SELECT data FROM objects WHERE id = ?1", [&id.0[..]], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
        let Some(c) = self.get_compressed(id)? else {
            return Ok(None);
        };
        let raw = object::decompress(&c)
            .map_err(|e| err!(Internal, "corrupt object {}: {e}", id.short()))?;
        let (kind, body) = object::parse_raw(&raw)
            .ok_or_else(|| err!(Internal, "corrupt object {}", id.short()))?;
        Ok(Some((kind, body.to_vec())))
    }

    pub fn blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        match self.get(id)? {
            Some((Kind::Blob, b)) => Ok(b),
            Some(_) => fail!(Internal, "object {} is not a blob", id.short()),
            None => fail!(ObjectMissing, "object {} is not available", id.short()),
        }
    }

    pub fn tree(&self, id: &ObjectId) -> Result<Vec<TreeEntry>> {
        match self.get(id)? {
            Some((Kind::Tree, b)) => {
                object::decode_tree(&b).ok_or_else(|| err!(Internal, "corrupt tree {}", id.short()))
            }
            Some(_) => fail!(Internal, "object {} is not a tree", id.short()),
            None => fail!(ObjectMissing, "object {} is not available", id.short()),
        }
    }

    /// Delete objects not in `live` and older than `grace_ms`.
    /// Returns (count, bytes).
    pub fn sweep(&self, live: &HashSet<ObjectId>, grace_ms: i64) -> Result<(u64, u64)> {
        let cutoff = ghf_proto::id::now_ms() - grace_ms;
        let candidates: Vec<(Vec<u8>, i64)> = {
            let mut st = self
                .db
                .prepare("SELECT id, length(data) FROM objects WHERE created_at <= ?1")?;
            let rows = st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let (mut n, mut bytes) = (0, 0);
        self.tx(|| {
            for (id, len) in &candidates {
                let Ok(arr) = <[u8; 32]>::try_from(id.as_slice()) else {
                    continue;
                };
                if live.contains(&ObjectId(arr)) {
                    continue;
                }
                self.db.execute("DELETE FROM objects WHERE id = ?1", [id])?;
                self.db
                    .execute("DELETE FROM remote_objects WHERE id = ?1", [id])?;
                n += 1;
                bytes += *len as u64;
            }
            Ok(())
        })?;
        Ok((n, bytes))
    }

    /// Objects the server is known to hold (with their full closure).
    pub fn remote_has(&self, id: &ObjectId) -> bool {
        self.db
            .query_row(
                "SELECT 1 FROM remote_objects WHERE id = ?1",
                [&id.0[..]],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn mark_remote(&self, ids: &[ObjectId]) -> Result<()> {
        self.tx(|| {
            let mut st = self
                .db
                .prepare("INSERT OR IGNORE INTO remote_objects (id) VALUES (?1)")?;
            for id in ids {
                st.execute([&id.0[..]])?;
            }
            Ok(())
        })
    }

    pub fn forget_remote(&self) -> Result<()> {
        self.db.execute("DELETE FROM remote_objects", [])?;
        Ok(())
    }

    // -- traces ---------------------------------------------------------------

    fn trace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TraceRow> {
        let json: String = row.get(0)?;
        let record: TraceRecord = serde_json::from_str(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(TraceRow {
            record,
            writer: row.get(1)?,
            lease_at: row.get(2)?,
        })
    }

    pub fn insert_trace(&self, t: &TraceRecord, dirty: bool) -> Result<()> {
        self.db.execute(
            "INSERT INTO traces (id, record, state, context_id, created_at, dirty) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![t.id, serde_json::to_string(t)?, t.state.as_str(), t.context_id, t.created_at, dirty as i64],
        )?;
        Ok(())
    }

    /// Replace a trace record. `dirty` marks it for upload.
    pub fn update_trace(&self, t: &TraceRecord, dirty: bool) -> Result<()> {
        self.db.execute(
            "UPDATE traces SET record = ?2, state = ?3, dirty = MAX(dirty, ?4) WHERE id = ?1",
            params![
                t.id,
                serde_json::to_string(t)?,
                t.state.as_str(),
                dirty as i64
            ],
        )?;
        Ok(())
    }

    pub fn trace(&self, id: &str) -> Result<Option<TraceRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT record, writer, lease_at FROM traces WHERE id = ?1",
                [id],
                Self::trace_from_row,
            )
            .optional()?)
    }

    pub fn open_trace(&self, context_id: &str) -> Result<Option<TraceRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT record, writer, lease_at FROM traces WHERE context_id = ?1 AND state = 'open' ORDER BY created_at DESC LIMIT 1",
                [context_id],
                Self::trace_from_row,
            )
            .optional()?)
    }

    /// Most recent trace of a context, any state but pruned.
    pub fn latest_trace(&self, context_id: &str) -> Result<Option<TraceRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT record, writer, lease_at FROM traces WHERE context_id = ?1 AND state != 'pruned' ORDER BY (state = 'open') DESC, created_at DESC LIMIT 1",
                [context_id],
                Self::trace_from_row,
            )
            .optional()?)
    }

    pub fn traces(&self) -> Result<Vec<TraceRow>> {
        let mut st = self
            .db
            .prepare("SELECT record, writer, lease_at FROM traces ORDER BY created_at")?;
        let rows = st.query_map([], Self::trace_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn trace_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self
            .db
            .prepare("SELECT id FROM traces WHERE substr(id, 1, length(?1)) = ?1 LIMIT 3")?;
        let rows = st.query_map([prefix], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_writer(&self, trace_id: &str, writer: Option<&str>, lease_at: i64) -> Result<()> {
        self.db.execute(
            "UPDATE traces SET writer = ?2, lease_at = ?3 WHERE id = ?1",
            params![trace_id, writer, lease_at],
        )?;
        Ok(())
    }

    pub fn dirty_traces(&self) -> Result<Vec<TraceRecord>> {
        let mut st = self.db.prepare(
            "SELECT record, writer, lease_at FROM traces WHERE dirty = 1 ORDER BY created_at",
        )?;
        let rows = st.query_map([], Self::trace_from_row)?;
        Ok(rows
            .map(|r| r.map(|t| t.record))
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Clear the dirty flag if the record has not changed since `json`.
    pub fn mark_trace_clean(&self, t: &TraceRecord) -> Result<()> {
        self.db.execute(
            "UPDATE traces SET dirty = 0 WHERE id = ?1 AND record = ?2",
            params![t.id, serde_json::to_string(t)?],
        )?;
        Ok(())
    }

    pub fn delete_trace_rows(&self, trace_id: &str, keep: &[String]) -> Result<()> {
        self.tx(|| {
            let mut st = self
                .db
                .prepare("SELECT id FROM checkpoints WHERE trace_id = ?1")?;
            let ids: Vec<String> = st
                .query_map([trace_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for id in ids.iter().filter(|i| !keep.contains(i)) {
                self.db
                    .execute("DELETE FROM checkpoints WHERE id = ?1", [id])?;
            }
            Ok(())
        })
    }

    // -- checkpoints ----------------------------------------------------------

    fn ck_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRecord> {
        let json: String = row.get(0)?;
        serde_json::from_str(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    }

    pub fn insert_checkpoint(&self, c: &CheckpointRecord, synced: bool) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO checkpoints (id, trace_id, seq, record, created_at, synced) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![c.id, c.trace_id, c.seq, serde_json::to_string(c)?, c.created_at, synced as i64],
        )?;
        Ok(())
    }

    pub fn checkpoint(&self, id: &str) -> Result<Option<CheckpointRecord>> {
        Ok(self
            .db
            .query_row(
                "SELECT record FROM checkpoints WHERE id = ?1",
                [id],
                Self::ck_from_row,
            )
            .optional()?)
    }

    pub fn checkpoint_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self
            .db
            .prepare("SELECT id FROM checkpoints WHERE substr(id, 1, length(?1)) = ?1 LIMIT 3")?;
        let rows = st.query_map([prefix], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn latest_checkpoint(&self, trace_id: &str) -> Result<Option<CheckpointRecord>> {
        Ok(self
            .db
            .query_row(
                "SELECT record FROM checkpoints WHERE trace_id = ?1 ORDER BY seq DESC LIMIT 1",
                [trace_id],
                Self::ck_from_row,
            )
            .optional()?)
    }

    pub fn checkpoints(&self, trace_id: &str) -> Result<Vec<CheckpointRecord>> {
        let mut st = self
            .db
            .prepare("SELECT record FROM checkpoints WHERE trace_id = ?1 ORDER BY seq")?;
        let rows = st.query_map([trace_id], Self::ck_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn all_checkpoints(&self) -> Result<Vec<CheckpointRecord>> {
        let mut st = self.db.prepare("SELECT record FROM checkpoints")?;
        let rows = st.query_map([], Self::ck_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn checkpoint_count(&self, trace_id: &str) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE trace_id = ?1",
            [trace_id],
            |r| r.get(0),
        )?)
    }

    pub fn unsynced(&self) -> Result<Vec<CheckpointRecord>> {
        let mut st = self
            .db
            .prepare("SELECT record FROM checkpoints WHERE synced = 0 ORDER BY created_at, seq")?;
        let rows = st.query_map([], Self::ck_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unsynced_count(&self) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE synced = 0",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn unsynced_in_trace(&self, trace_id: &str) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE synced = 0 AND trace_id = ?1",
            [trace_id],
            |r| r.get(0),
        )?)
    }

    /// The server refused this checkpoint permanently. It stays restorable
    /// locally but is no longer retried (`synced = 2`).
    pub fn mark_rejected(&self, id: &str) -> Result<()> {
        self.db
            .execute("UPDATE checkpoints SET synced = 2 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn rejected_count(&self) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE synced = 2",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn mark_synced(&self, id: &str) -> Result<()> {
        self.db
            .execute("UPDATE checkpoints SET synced = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    // -- holds ------------------------------------------------------------------

    pub fn add_hold_op(
        &self,
        trace_id: &str,
        name: &str,
        place: bool,
        expires_at: Option<i64>,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO hold_ops (trace_id, name, place, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![trace_id, name, place as i64, expires_at],
        )?;
        Ok(())
    }

    /// Pending hold operations, oldest first.
    pub fn hold_ops(&self) -> Result<Vec<HoldOp>> {
        let mut st = self
            .db
            .prepare("SELECT id, trace_id, name, place, expires_at FROM hold_ops ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get(4)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_hold_op(&self, id: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM hold_ops WHERE id = ?1", [id])?;
        Ok(())
    }

    // -- failures -----------------------------------------------------------------

    pub fn record_failure(
        &self,
        context_id: Option<&str>,
        code: &str,
        message: &str,
        strict: bool,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO failures (at, context_id, code, message, strict) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ghf_proto::id::now_ms(), context_id, code, message, strict as i64],
        )?;
        Ok(())
    }

    /// Failures since `since` (unix ms): (at, code, message, strict).
    pub fn failures_since(&self, since: i64) -> Result<Vec<(i64, String, String, bool)>> {
        let mut st = self.db.prepare(
            "SELECT at, code, message, strict FROM failures WHERE at >= ?1 ORDER BY at DESC LIMIT 20",
        )?;
        let rows = st.query_map([since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // -- stat cache -----------------------------------------------------------------

    pub fn load_statcache(&self, context_id: &str) -> Result<HashMap<String, StatEntry>> {
        let mut st = self.db.prepare(
            "SELECT path, size, mtime, ino, mode, id FROM statcache WHERE context_id = ?1",
        )?;
        let rows = st.query_map([context_id], |r| {
            let path: String = r.get(0)?;
            let mode: String = r.get(4)?;
            let id: String = r.get(5)?;
            Ok((
                path,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? as u64,
                mode,
                id,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, size, mtime, ino, mode, id) = row?;
            if let Some(id) = ObjectId::from_hex(&id) {
                out.insert(
                    path,
                    StatEntry {
                        size,
                        mtime,
                        ino,
                        mode: parse_mode(&mode),
                        id,
                    },
                );
            }
        }
        Ok(out)
    }

    pub fn save_statcache(
        &self,
        context_id: &str,
        upserts: &[(String, StatEntry)],
        removed: &[String],
    ) -> Result<()> {
        if upserts.is_empty() && removed.is_empty() {
            return Ok(());
        }
        self.tx(|| {
            let mut up = self.db.prepare(
                "INSERT OR REPLACE INTO statcache (context_id, path, size, mtime, ino, mode, id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (path, e) in upserts {
                up.execute(params![
                    context_id,
                    path,
                    e.size as i64,
                    e.mtime,
                    e.ino as i64,
                    mode_str(e.mode),
                    e.id.to_hex()
                ])?;
            }
            let mut del = self
                .db
                .prepare("DELETE FROM statcache WHERE context_id = ?1 AND path = ?2")?;
            for path in removed {
                del.execute(params![context_id, path])?;
            }
            Ok(())
        })
    }

    // -- idempotency and kv ------------------------------------------------------------

    pub fn idempotent_response(&self, key: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row(
                "SELECT fingerprint, response FROM idempotency WHERE key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn save_idempotent(&self, key: &str, fingerprint: &str, response: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO idempotency (key, fingerprint, response, at) VALUES (?1, ?2, ?3, ?4)",
            params![key, fingerprint, response, ghf_proto::id::now_ms()],
        )?;
        Ok(())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        self.db.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }
}
