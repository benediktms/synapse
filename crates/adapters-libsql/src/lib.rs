use std::path::Path;

use domain::{
    EditRequest, Error, Link, Memory, MemoryId, MemoryKind, Relation, Scope, ScopeFilter, Store,
    Timestamp,
};
use libsql::{Builder, Connection, Database, Value, params};

/// Final consolidated schema, equivalent to adapters-sqlite's sqlx migrations 0001-0004.
const SCHEMA: &str = r#"
CREATE TABLE memories (
    id              TEXT PRIMARY KEY NOT NULL,
    content         TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('user','feedback','project','reference')),
    scope           TEXT NOT NULL,
    tags            TEXT NOT NULL DEFAULT '[]',
    pinned          INTEGER NOT NULL DEFAULT 0,
    embedding       BLOB NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    importance      INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    embedding_model TEXT NOT NULL,
    embedding_dim   INTEGER NOT NULL
);
CREATE VIRTUAL TABLE memories_fts USING fts5(
    content, content='memories', content_rowid='rowid'
);
CREATE TRIGGER memories_fts_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER memories_fts_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER memories_fts_after_update AFTER UPDATE OF content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TABLE links (
    low_id     TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    high_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL CHECK (relation IN ('relation','support','contradiction','supersession')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (low_id, high_id, relation)
);
CREATE INDEX links_low  ON links(low_id);
CREATE INDEX links_high ON links(high_id);
"#;

#[derive(Debug)]
pub struct LibsqlStore {
    db: Database,
    dim: usize,
    /// Unix seconds of the last successful sync (0 if never).
    last_sync_ok: std::sync::atomic::AtomicU64,
    /// Whether the last sync reached the primary (network was up).
    connected: std::sync::atomic::AtomicBool,
    /// Local writes since the last successful sync (offline outbox backlog proxy).
    dirty: std::sync::atomic::AtomicU64,
}

impl LibsqlStore {
    /// Open (or create) an offline-writable embedded replica of a Turso primary.
    pub async fn open(
        path: impl AsRef<Path>,
        url: String,
        auth_token: String,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let db = Builder::new_synced_database(path.as_ref(), url, auth_token)
            .build()
            .await
            .map_err(store_err)?;
        Self::init(db, embedding_model, embedding_dim).await
    }

    /// Apply schema + meta (mirroring adapters-sqlite's `open`) over an already-built Database.
    pub async fn init(
        db: Database,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let conn = db.connect().map_err(store_err)?;
        ensure_schema(&conn).await?;
        if !meta_exists(&conn).await? {
            let memories: i64 = scalar(&conn, "SELECT COUNT(*) FROM memories")
                .await
                .map_err(store_err)?;
            if memories > 0 {
                return Err(Error::Store(format!(
                    "{memories} memories but no embedding meta row: their vectors cannot be \
                     attributed to a model and will not be re-embedded — restore the database \
                     from a backup, or re-import its export into a fresh one"
                )));
            }
            conn.execute(
                "INSERT INTO meta (id, embedding_model, embedding_dim) VALUES (1, ?, ?) \
                 ON CONFLICT (id) DO NOTHING",
                params![
                    embedding_model,
                    i64::try_from(embedding_dim).map_err(store_err)?
                ],
            )
            .await
            .map_err(store_err)?;
        }
        let (model, dim) = embedding_meta(&conn).await?;
        let want_dim = i64::try_from(embedding_dim).map_err(store_err)?;
        if model != embedding_model || dim != want_dim {
            return Err(Error::Store(format!(
                "embedding meta mismatch: db has {} ({} dims), runtime uses {} ({} dims)",
                model, dim, embedding_model, embedding_dim
            )));
        }
        Ok(Self {
            db,
            dim: embedding_dim,
            last_sync_ok: std::sync::atomic::AtomicU64::new(0),
            connected: std::sync::atomic::AtomicBool::new(false),
            dirty: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Push local WAL frames (outbox) to the primary and pull remote frames down.
    /// Fails open on network errors: an unreachable remote degrades to a no-op, so
    /// offline reads/writes are never blocked by a failed sync.
    pub async fn sync(&self) -> Result<(), Error> {
        let rep = self.db.sync().await.map_err(store_err)?;
        // On a failed-open HTTP dispatch, libsql returns Replicated { frame_no: None, .. }.
        let reached = rep.frame_no().is_some();
        self.connected
            .store(reached, std::sync::atomic::Ordering::Relaxed);
        if reached {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            self.last_sync_ok
                .store(now, std::sync::atomic::Ordering::Relaxed);
            self.dirty.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    fn mark_dirty(&self) {
        self.dirty
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Unix seconds of the last successful sync (0 if none).
    pub fn last_synced_at(&self) -> u64 {
        self.last_sync_ok.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the last sync reached the primary.
    pub fn online(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Local writes not yet pushed to the primary (offline outbox backlog proxy).
    pub fn pending_outbox(&self) -> usize {
        self.dirty.load(std::sync::atomic::Ordering::Relaxed) as usize
    }

    fn conn(&self) -> Result<Connection, Error> {
        self.db.connect().map_err(store_err)
    }
}

impl Store for LibsqlStore {
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare(
                "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at \
                 FROM memories WHERE id = ?",
            )
            .await
            .map_err(store_err)?;
        let mut rows = stmt.query(params![id.as_str()]).await.map_err(store_err)?;
        if let Some(row) = rows.next().await.map_err(store_err)? {
            Ok(Some(memory_from_row(&row)?))
        } else {
            Ok(None)
        }
    }

    async fn get_with_embedding(&self, id: &MemoryId) -> Result<Option<(Memory, Vec<f32>)>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare(
                "SELECT id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at \
                 FROM memories WHERE id = ?",
            )
            .await
            .map_err(store_err)?;
        let mut rows = stmt.query(params![id.as_str()]).await.map_err(store_err)?;
        if let Some(row) = rows.next().await.map_err(store_err)? {
            let embedding = decode_embedding(&row.get::<Vec<u8>>(7).map_err(store_err)?, self.dim)?;
            let memory = Memory {
                id: id.clone(),
                content: row.get::<String>(1).map_err(store_err)?,
                kind: MemoryKind::parse(&row.get::<String>(2).map_err(store_err)?)?,
                scope: Scope::parse(&row.get::<String>(3).map_err(store_err)?)?,
                tags: serde_json::from_str(&row.get::<String>(4).map_err(store_err)?)
                    .map_err(store_err)?,
                pinned: row.get::<i64>(5).map_err(store_err)? != 0,
                importance: domain::Importance::from_rank(row.get::<i64>(6).map_err(store_err)?),
                created_at: Timestamp::new(row.get::<String>(8).map_err(store_err)?),
                updated_at: Timestamp::new(row.get::<String>(9).map_err(store_err)?),
            };
            Ok(Some((memory, embedding)))
        } else {
            Ok(None)
        }
    }

    async fn insert(&self, memory: &Memory, embedding: &[f32]) -> Result<(), Error> {
        let conn = self.conn()?;
        let blob = Value::Blob(encode_embedding(embedding, self.dim)?);
        let tags = serde_json::to_string(&memory.tags).map_err(store_err)?;
        let affected = conn
            .execute(
                "INSERT INTO memories (id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
                params![
                    memory.id.as_str(),
                    memory.content.as_str(),
                    memory.kind.as_str(),
                    memory.scope.as_str(),
                    tags.as_str(),
                    i64::from(memory.pinned),
                    i64::from(memory.importance.rank()),
                    blob,
                    memory.created_at.as_str(),
                    memory.updated_at.as_str(),
                ],
            )
            .await
            .map_err(store_err)?;
        if affected == 1 {
            return Ok(());
        }
        self.mark_dirty();
        let existing = self
            .get(&memory.id)
            .await?
            .ok_or_else(|| Error::Store(format!("memory {} vanished during insert", memory.id)))?;
        let same_payload = existing.content == memory.content
            && existing.kind == memory.kind
            && existing.scope == memory.scope
            && existing.tags == memory.tags
            && existing.pinned == memory.pinned
            && existing.importance == memory.importance;
        if same_payload {
            Ok(())
        } else {
            Err(Error::Conflict(memory.id.clone()))
        }
    }

    async fn update(
        &self,
        id: &MemoryId,
        patch: &EditRequest,
        embedding: Option<&[f32]>,
        now: &Timestamp,
    ) -> Result<Memory, Error> {
        let conn = self.conn()?;
        let content = patch.content.as_deref().map(|s| Value::Text(s.to_string()));
        let tags = patch
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(store_err)?
            .map(Value::Text);
        let pinned = patch.pinned.map(|v| Value::Integer(i64::from(v)));
        let importance = patch
            .importance
            .map(|tier| Value::Integer(i64::from(tier.rank())));
        let embedding = match embedding {
            Some(e) => Value::Blob(encode_embedding(e, self.dim)?),
            None => Value::Null,
        };
        let updated_at = Value::Text(now.to_string());
        let affected = conn
            .execute(
                r#"UPDATE memories SET content = COALESCE(?, content), tags = COALESCE(?, tags),
                       pinned = COALESCE(?, pinned), importance = COALESCE(?, importance),
                       embedding = COALESCE(?, embedding), updated_at = ?
                   WHERE id = ?"#,
                libsql::params_from_iter([
                    content.unwrap_or(Value::Null),
                    tags.unwrap_or(Value::Null),
                    pinned.unwrap_or(Value::Null),
                    importance.unwrap_or(Value::Null),
                    embedding,
                    updated_at,
                    Value::Text(id.to_string()),
                ]),
            )
            .await
            .map_err(store_err)?;
        if affected == 0 {
            return Err(Error::NotFound(id.clone()));
        }
        self.mark_dirty();
        self.get(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.clone()))
    }

    async fn delete(&self, id: &MemoryId) -> Result<bool, Error> {
        let conn = self.conn()?;
        let affected = conn
            .execute("DELETE FROM memories WHERE id = ?", params![id.as_str()])
            .await
            .map_err(store_err)?;
        if affected > 0 {
            self.mark_dirty();
        }
        Ok(affected > 0)
    }

    async fn list(&self) -> Result<Vec<Memory>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare(
                "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at \
                 FROM memories",
            )
            .await
            .map_err(store_err)?;
        let mut rows = stmt.query(params![]).await.map_err(store_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(store_err)? {
            out.push(memory_from_row(&row)?);
        }
        Ok(out)
    }

    async fn embeddings(&self, filter: &ScopeFilter) -> Result<Vec<(MemoryId, Vec<f32>)>, Error> {
        let conn = self.conn()?;
        let project = filter.project.as_deref().unwrap_or("");
        let stmt = conn
            .prepare("SELECT id, embedding FROM memories WHERE scope = 'workspace' OR scope = ?")
            .await
            .map_err(store_err)?;
        let mut rows = stmt.query(params![project]).await.map_err(store_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(store_err)? {
            let id = MemoryId::parse(&row.get::<String>(0).map_err(store_err)?)?;
            let embedding = decode_embedding(&row.get::<Vec<u8>>(1).map_err(store_err)?, self.dim)?;
            out.push((id, embedding));
        }
        Ok(out)
    }

    async fn keyword_search(
        &self,
        query: &str,
        filter: &ScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryId>, Error> {
        let Some(fts_query) = escape_fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn()?;
        let project = filter.project.as_deref().unwrap_or("");
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let stmt = conn
            .prepare(
                r#"SELECT m.id FROM memories_fts
                   JOIN memories m ON m.rowid = memories_fts.rowid
                   WHERE memories_fts MATCH ? AND (m.scope = 'workspace' OR m.scope = ?)
                   ORDER BY memories_fts.rank
                   LIMIT ?"#,
            )
            .await
            .map_err(store_err)?;
        let mut rows = stmt
            .query(params![fts_query, project, limit])
            .await
            .map_err(store_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(store_err)? {
            out.push(MemoryId::parse(&row.get::<String>(0).map_err(store_err)?)?);
        }
        Ok(out)
    }

    async fn insert_link(&self, link: &Link) -> Result<(), Error> {
        let canonical = link.clone().canonical();
        if self.get(&canonical.source).await?.is_none() {
            return Err(Error::NotFound(canonical.source));
        }
        if self.get(&canonical.target).await?.is_none() {
            return Err(Error::NotFound(canonical.target));
        }
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO links (low_id, high_id, relation) VALUES (?, ?, ?)",
            params![
                canonical.source.as_str(),
                canonical.target.as_str(),
                canonical.relation.as_str(),
            ],
        )
        .await
        .map_err(store_err)?;
        self.mark_dirty();
        Ok(())
    }

    async fn delete_links_between(&self, a: &MemoryId, b: &MemoryId) -> Result<usize, Error> {
        let conn = self.conn()?;
        let affected = conn
            .execute(
                "DELETE FROM links WHERE (low_id = ? AND high_id = ?) OR (low_id = ? AND high_id = ?)",
                params![a.as_str(), b.as_str(), b.as_str(), a.as_str()],
            )
            .await
            .map_err(store_err)?;
        if affected > 0 {
            self.mark_dirty();
        }
        usize::try_from(affected).map_err(store_err)
    }

    async fn links_of(&self, id: &MemoryId) -> Result<Vec<Link>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare("SELECT low_id, high_id, relation FROM links WHERE low_id = ? OR high_id = ?")
            .await
            .map_err(store_err)?;
        let mut rows = stmt
            .query(params![id.as_str(), id.as_str()])
            .await
            .map_err(store_err)?;
        links_from_rows(&mut rows).await
    }

    async fn links_all(&self) -> Result<Vec<Link>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare("SELECT low_id, high_id, relation FROM links")
            .await
            .map_err(store_err)?;
        let mut rows = stmt.query(params![]).await.map_err(store_err)?;
        links_from_rows(&mut rows).await
    }
}

struct MemoryRow {
    id: String,
    content: String,
    kind: String,
    scope: String,
    tags: String,
    pinned: i64,
    importance: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<MemoryRow> for Memory {
    type Error = Error;

    fn try_from(row: MemoryRow) -> Result<Self, Error> {
        Ok(Memory {
            id: MemoryId::parse(&row.id)?,
            content: row.content,
            kind: MemoryKind::parse(&row.kind)?,
            scope: Scope::parse(&row.scope)?,
            tags: serde_json::from_str(&row.tags).map_err(store_err)?,
            pinned: row.pinned != 0,
            importance: domain::Importance::from_rank(row.importance),
            created_at: Timestamp::new(row.created_at),
            updated_at: Timestamp::new(row.updated_at),
        })
    }
}

/// Read the 9 non-embedding memory columns (indices 0-8) from a plain SELECT row.
fn memory_from_row(row: &libsql::Row) -> Result<Memory, Error> {
    MemoryRow {
        id: row.get::<String>(0).map_err(store_err)?,
        content: row.get::<String>(1).map_err(store_err)?,
        kind: row.get::<String>(2).map_err(store_err)?,
        scope: row.get::<String>(3).map_err(store_err)?,
        tags: row.get::<String>(4).map_err(store_err)?,
        pinned: row.get::<i64>(5).map_err(store_err)?,
        importance: row.get::<i64>(6).map_err(store_err)?,
        created_at: row.get::<String>(7).map_err(store_err)?,
        updated_at: row.get::<String>(8).map_err(store_err)?,
    }
    .try_into()
}

async fn links_from_rows(rows: &mut libsql::Rows) -> Result<Vec<Link>, Error> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(store_err)? {
        out.push(Link {
            source: MemoryId::parse(&row.get::<String>(0).map_err(store_err)?)?,
            target: MemoryId::parse(&row.get::<String>(1).map_err(store_err)?)?,
            relation: Relation::parse(&row.get::<String>(2).map_err(store_err)?)?,
        });
    }
    Ok(out)
}

async fn ensure_schema(conn: &Connection) -> Result<(), Error> {
    let tables: i64 = scalar(
        conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories'",
    )
    .await
    .map_err(store_err)?;
    if tables == 0 {
        conn.execute_batch(SCHEMA).await.map_err(store_err)?;
    }
    Ok(())
}

async fn meta_exists(conn: &Connection) -> Result<bool, Error> {
    Ok(scalar(conn, "SELECT COUNT(*) FROM meta WHERE id = 1").await? > 0)
}

async fn embedding_meta(conn: &Connection) -> Result<(String, i64), Error> {
    let stmt = conn
        .prepare("SELECT embedding_model, embedding_dim FROM meta WHERE id = 1")
        .await
        .map_err(store_err)?;
    let mut rows = stmt.query(params![]).await.map_err(store_err)?;
    let row = rows
        .next()
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::Store("no embedding meta".into()))?;
    Ok((
        row.get::<String>(0).map_err(store_err)?,
        row.get::<i64>(1).map_err(store_err)?,
    ))
}

/// Run a scalar query that returns a single i64 in the first column of the first row.
async fn scalar(conn: &Connection, sql: &str) -> Result<i64, Error> {
    let stmt = conn.prepare(sql).await.map_err(store_err)?;
    let mut rows = stmt.query(params![]).await.map_err(store_err)?;
    let row = rows
        .next()
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::Store("scalar query returned no rows".into()))?;
    row.get::<i64>(0).map_err(store_err)
}

fn encode_embedding(embedding: &[f32], dim: usize) -> Result<Vec<u8>, Error> {
    if embedding.len() != dim {
        return Err(Error::Store(format!(
            "embedding has {} dims, meta says {dim}",
            embedding.len()
        )));
    }
    Ok(embedding.iter().flat_map(|v| v.to_le_bytes()).collect())
}

fn decode_embedding(blob: &[u8], dim: usize) -> Result<Vec<f32>, Error> {
    if blob.len() != dim * 4 {
        return Err(Error::Store(format!(
            "embedding blob is {} bytes, meta dimension {dim} needs {}",
            blob.len(),
            dim * 4
        )));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)")))
        .collect())
}

fn escape_fts_query(raw: &str) -> Option<String> {
    let phrases: Vec<String> = raw
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect();
    if phrases.is_empty() {
        None
    } else {
        Some(phrases.join(" OR "))
    }
}

fn store_err(err: impl std::fmt::Display) -> Error {
    Error::Store(err.to_string())
}

/// Minimal Turso Cloud platform client: enumerating and creating databases in an org.
pub struct TursoPlatform {
    client: reqwest::Client,
}

#[derive(Clone, Debug)]
pub struct TursoDb {
    pub name: String,
    pub url: String,
}

#[derive(serde::Deserialize)]
struct RawDb {
    #[serde(rename = "Name", alias = "name")]
    name: String,
    #[serde(rename = "Hostname", alias = "hostname")]
    hostname: String,
    #[serde(rename = "libsql_url", default)]
    libsql_url: Option<String>,
}

impl RawDb {
    fn into_db(self) -> TursoDb {
        let url = self
            .libsql_url
            .unwrap_or_else(|| format!("libsql://{}", self.hostname));
        TursoDb {
            name: self.name,
            url,
        }
    }
}

impl Default for TursoPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl TursoPlatform {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn list_databases(&self, org: &str, token: &str) -> Result<Vec<TursoDb>, Error> {
        #[derive(serde::Deserialize)]
        struct ListResp {
            #[serde(rename = "databases", default)]
            databases: Vec<RawDb>,
        }
        let resp = self
            .client
            .get(format!(
                "https://api.turso.tech/v1/organizations/{org}/databases"
            ))
            .bearer_auth(token)
            .send()
            .await
            .map_err(store_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(store_err)?;
        if !status.is_success() {
            return Err(Error::Store(format!(
                "turso list databases failed ({status}): {body}"
            )));
        }
        let parsed: ListResp = serde_json::from_str(&body).map_err(store_err)?;
        Ok(parsed.databases.into_iter().map(RawDb::into_db).collect())
    }

    pub async fn create_database(
        &self,
        org: &str,
        token: &str,
        name: &str,
    ) -> Result<TursoDb, Error> {
        #[derive(serde::Deserialize)]
        struct CreateResp {
            #[serde(rename = "database")]
            database: RawDb,
        }
        let resp = self
            .client
            .post(format!(
                "https://api.turso.tech/v1/organizations/{org}/databases"
            ))
            .bearer_auth(token)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(store_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(store_err)?;
        if !status.is_success() {
            return Err(Error::Store(format!(
                "turso create database failed ({status}): {body}"
            )));
        }
        let parsed: CreateResp = serde_json::from_str(&body).map_err(store_err)?;
        Ok(parsed.database.into_db())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn offline_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        // A plain local libSQL DB (same engine, same Connection API, no replication) exercises
        // the identical Store SQL/schema path fully offline.
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let store = LibsqlStore::init(db, "test-model", 4).await.unwrap();
        let memory = Memory {
            id: MemoryId::parse("m_0000000000000000000000").unwrap(),
            content: "deploy staging offers".to_string(),
            kind: MemoryKind::parse("reference").unwrap(),
            scope: Scope::parse("workspace").unwrap(),
            tags: vec!["deploy".to_string()],
            pinned: false,
            importance: domain::Importance::from_rank(1),
            created_at: Timestamp::new("2026-08-07T00:00:00Z".to_string()),
            updated_at: Timestamp::new("2026-08-07T00:00:00Z".to_string()),
        };
        let embedding = vec![0.1f32, 0.2, 0.3, 0.4];
        store.insert(&memory, &embedding).await.unwrap();
        let got = store.get(&memory.id).await.unwrap().unwrap();
        assert_eq!(got.content, memory.content);
        assert_eq!(got.tags, memory.tags);
        let hits = store
            .keyword_search("deploy", &ScopeFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits, vec![memory.id.clone()]);
    }

    #[test]
    fn escape_quotes_each_whitespace_token() {
        assert_eq!(
            escape_fts_query("deploy staging offers").unwrap(),
            r#""deploy" OR "staging" OR "offers""#
        );
    }

    #[test]
    fn embedding_roundtrips_le_f32() {
        let original = vec![1.0f32, -0.5, 0.0, 3.25];
        let blob = encode_embedding(&original, 4).unwrap();
        assert_eq!(blob.len(), 16);
        assert_eq!(decode_embedding(&blob, 4).unwrap(), original);
    }
}
