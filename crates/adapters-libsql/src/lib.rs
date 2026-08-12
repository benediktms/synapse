use std::path::Path;

use domain::{
    EditRequest, Error, Link, Memory, MemoryId, MemoryKind, Relation, Scope, ScopeFilter, Store,
    Timestamp, trim_keyword_tail,
};
use libsql::{Builder, Connection, Database, Value, params};

/// Final consolidated schema, equivalent to adapters-sqlite's sqlx migrations 0001-0006.
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
    importance      INTEGER NOT NULL DEFAULT 1,
    title           TEXT NOT NULL DEFAULT ''
);
CREATE TABLE meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    embedding_model TEXT NOT NULL,
    embedding_dim   INTEGER NOT NULL
);
CREATE VIRTUAL TABLE memories_fts USING fts5(
    title, content, content='memories', content_rowid='rowid'
);
CREATE TRIGGER memories_fts_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
END;
CREATE TRIGGER memories_fts_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
END;
CREATE TRIGGER memories_fts_after_update AFTER UPDATE OF title, content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
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

/// Rebuild of `memories_fts` over both indexed columns, for replicas that predate the change.
const FTS_TITLE_DDL: &str = r#"
DROP TRIGGER IF EXISTS memories_fts_after_insert;
DROP TRIGGER IF EXISTS memories_fts_after_delete;
DROP TRIGGER IF EXISTS memories_fts_after_update;
DROP TABLE IF EXISTS memories_fts;
CREATE VIRTUAL TABLE memories_fts USING fts5(
    title, content, content='memories', content_rowid='rowid'
);
CREATE TRIGGER memories_fts_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
END;
CREATE TRIGGER memories_fts_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
END;
CREATE TRIGGER memories_fts_after_update AFTER UPDATE OF title, content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
END;
INSERT INTO memories_fts(memories_fts) VALUES ('rebuild');
"#;

#[derive(Debug)]
pub struct LibsqlStore {
    db: Database,
    dim: usize,
    /// Unix seconds of the last successful sync (0 if never).
    last_sync_ok: std::sync::atomic::AtomicU64,
    /// Whether the last sync reached the primary (network was up).
    connected: std::sync::atomic::AtomicBool,
    /// What the last failed sync said, so status can name an auth failure instead of
    /// letting it masquerade as a network outage.
    last_error: std::sync::Mutex<Option<String>>,
}

impl LibsqlStore {
    /// Open (or create) a remote-first embedded replica of a Turso primary.
    ///
    /// Reads use the local replica. Mutations are committed by the primary before
    /// returning and then reflected locally, so an acknowledged write is visible to
    /// every machine after its next pull. The CLI's durable outbox owns offline saves.
    ///
    /// A replica that fails `corruption` is set aside and pulled again rather than served.
    /// Remote-first writes are what make that safe: the file holds nothing the primary does
    /// not already have, so the rebuild costs a pull and never data.
    pub async fn open(
        path: impl AsRef<Path>,
        url: String,
        auth_token: String,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let existed = path.exists();
        let build = async || {
            Builder::new_remote_replica(path, url.clone(), auth_token.clone())
                .read_your_writes(true)
                .build()
                .await
                .map_err(store_err)
        };

        let db = build().await?;
        // Fail open for an existing replica: reads remain available and the background
        // sync retries. A fresh replica still needs the primary to initialize its schema.
        let _ = db.sync().await;

        if let Some(damage) = corruption(&db, existed).await {
            tracing::warn!("rebuilding {}: {damage}", path.display());
            drop(db);
            quarantine_replica_files(path)?;
            let db = build().await?;
            db.sync().await.map_err(|e| {
                store_err(format!(
                    "replica {} is corrupt ({damage}) and the primary could not be \
                     reached to rebuild it: {e} — the corrupt files are kept beside it",
                    path.display()
                ))
            })?;
            return Self::init(db, embedding_model, embedding_dim).await;
        }
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
            last_error: std::sync::Mutex::new(None),
        })
    }

    /// Pull committed primary frames into the local read replica.
    /// Mutations already reached the primary before their Store operation returned.
    pub async fn sync(&self) -> Result<(), Error> {
        match self.db.sync().await {
            Ok(_) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.connected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.last_sync_ok
                    .store(now, std::sync::atomic::Ordering::Relaxed);
                *self.last_error.lock().expect("last_error lock") = None;
                Ok(())
            }
            Err(e) => {
                self.connected
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let err = store_err(e);
                *self.last_error.lock().expect("last_error lock") = Some(err.to_string());
                Err(err)
            }
        }
    }

    /// Unix seconds of the last successful sync (0 if none).
    pub fn last_synced_at(&self) -> u64 {
        self.last_sync_ok.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the last sync reached the primary.
    pub fn online(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What the last failed sync said; None after a successful sync.
    pub fn last_sync_error(&self) -> Option<String> {
        self.last_error.lock().expect("last_error lock").clone()
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
                "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at, title \
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
                "SELECT id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at, title \
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
                title: row.get::<String>(10).map_err(store_err)?,
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
                "INSERT INTO memories (id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at, title) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
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
                    memory.title.as_str(),
                ],
            )
            .await
            .map_err(store_err)?;
        if affected == 1 {
            return Ok(());
        }
        let existing = self
            .get(&memory.id)
            .await?
            .ok_or_else(|| Error::Store(format!("memory {} vanished during insert", memory.id)))?;
        let same_payload = existing.content == memory.content
            && existing.title == memory.title
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
        let title = patch.title.as_deref().map(|s| Value::Text(s.to_string()));
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
                r#"UPDATE memories SET content = COALESCE(?, content), title = COALESCE(?, title),
                       tags = COALESCE(?, tags),
                       pinned = COALESCE(?, pinned), importance = COALESCE(?, importance),
                       embedding = COALESCE(?, embedding), updated_at = ?
                   WHERE id = ?"#,
                libsql::params_from_iter([
                    content.unwrap_or(Value::Null),
                    title.unwrap_or(Value::Null),
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
        self.get(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.clone()))
    }

    async fn set_embedding(&self, id: &MemoryId, embedding: &[f32]) -> Result<(), Error> {
        let conn = self.conn()?;
        let blob = Value::Blob(encode_embedding(embedding, self.dim)?);
        let affected = conn
            .execute(
                "UPDATE memories SET embedding = ? WHERE id = ?",
                params![blob, id.as_str()],
            )
            .await
            .map_err(store_err)?;
        if affected == 0 {
            return Err(Error::NotFound(id.clone()));
        }
        Ok(())
    }

    async fn delete(&self, id: &MemoryId) -> Result<bool, Error> {
        let conn = self.conn()?;
        let affected = conn
            .execute("DELETE FROM memories WHERE id = ?", params![id.as_str()])
            .await
            .map_err(store_err)?;
        Ok(affected > 0)
    }

    async fn list(&self) -> Result<Vec<Memory>, Error> {
        let conn = self.conn()?;
        let stmt = conn
            .prepare(
                "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at, title \
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
                r#"SELECT m.id, memories_fts.rank FROM memories_fts
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
        let mut hits = Vec::new();
        while let Some(row) = rows.next().await.map_err(store_err)? {
            let id = MemoryId::parse(&row.get::<String>(0).map_err(store_err)?)?;
            hits.push((id, row.get::<f64>(1).map_err(store_err)?));
        }
        trim_keyword_tail(&mut hits, query);
        Ok(hits.into_iter().map(|(id, _)| id).collect())
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
    title: String,
}

impl TryFrom<MemoryRow> for Memory {
    type Error = Error;

    fn try_from(row: MemoryRow) -> Result<Self, Error> {
        Ok(Memory {
            id: MemoryId::parse(&row.id)?,
            content: row.content,
            title: row.title,
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

/// Read the 10 non-embedding memory columns (indices 0-9) from a plain SELECT row.
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
        title: row.get::<String>(9).map_err(store_err)?,
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

/// What is wrong with the replica on disk, or None when it can be served.
///
/// The probe is a read, so the replica connection answers it from the local file. `PRAGMA
/// quick_check` cannot be used: libsql's parser classifies it as a write (`StmtKind::Write`),
/// so a replica connection proxies it to the primary, which reports on the primary's file
/// instead of ours. A plain local handle would answer honestly but would recover and
/// checkpoint the WAL that libsql's sync layer owns.
///
/// A full scan of the table b-tree is what the pragma was reaching for anyway. `NOT INDEXED`
/// keeps the planner off the primary-key index, so damaged pages surface as a malformed-image
/// error and out-of-order rowids — the shape the real damage took — surface as a row count
/// that disagrees with the number of distinct ids it yielded.
///
/// `existed` says whether the replica file was on disk before this open. A replica that has
/// lost its schema is damage; a machine that has never opened one is not. Losing the schema is
/// how WAL damage presents under sync protocol v2: SQLite discards the unreadable WAL, and
/// because the replica keeps its pages there, what is left is a sound and empty database.
async fn corruption(db: &Database, existed: bool) -> Option<String> {
    let conn = match db.connect() {
        Ok(conn) => conn,
        Err(e) => return Some(e.to_string()),
    };
    let scan = conn
        .query(
            "SELECT COUNT(*), COUNT(DISTINCT id) FROM memories NOT INDEXED",
            params![],
        )
        .await;
    let mut rows = match scan {
        Ok(rows) => rows,
        Err(e) if e.to_string().contains("no such table") => {
            return existed.then(|| "the replica has no memories table".to_string());
        }
        Err(e) => return Some(e.to_string()),
    };
    let row = match rows.next().await {
        Ok(Some(row)) => row,
        Ok(None) => return Some("the replica did not answer a count of its memories".to_string()),
        Err(e) => return Some(e.to_string()),
    };
    match (row.get::<i64>(0), row.get::<i64>(1)) {
        (Ok(scanned), Ok(distinct)) if scanned == distinct => None,
        (Ok(scanned), Ok(distinct)) => Some(format!(
            "a scan of {scanned} memories yielded {distinct} distinct ids"
        )),
        (Err(e), _) | (_, Err(e)) => Some(e.to_string()),
    }
}

fn sibling(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Move every file the replica owns out of libsql's way so the next build bootstraps from the
/// primary. The files are renamed rather than deleted: a rebuild that cannot reach the primary
/// must not be the moment the only local copy disappears.
///
/// The replication metadata goes first and the database last. libsql refuses to open metadata
/// without a database, so a rename that fails part-way leaves the tolerated half, not the
/// rejected one.
fn quarantine_replica_files(path: &Path) -> Result<(), Error> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for suffix in ["-info", "-client_wal_index", "-journal", "-shm", "-wal", ""] {
        let from = sibling(path, suffix);
        let to = sibling(path, &format!("{suffix}.corrupt-{stamp}"));
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(store_err(format!(
                    "cannot set aside corrupt replica {}: {e}",
                    from.display()
                )));
            }
        }
    }
    Ok(())
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
        return Ok(());
    }
    add_missing_columns(conn).await?;
    widen_fts_to_title(conn).await
}

/// Replicas built before the index covered titles carry a one-column `memories_fts`. FTS5 cannot
/// gain a column in place, so the table and its triggers are rebuilt and the index repopulated
/// from `memories`.
async fn widen_fts_to_title(conn: &Connection) -> Result<(), Error> {
    let present = scalar(
        conn,
        "SELECT COUNT(*) FROM pragma_table_info('memories_fts') WHERE name = 'title'",
    )
    .await?;
    if present > 0 {
        return Ok(());
    }
    conn.execute_batch(FTS_TITLE_DDL).await.map_err(|e| {
        Error::Store(format!(
            "this replica indexes only the memory content and the primary did not widen it ({e}); \
             connect once and retry"
        ))
    })?;
    Ok(())
}

/// Replicas created before a column existed already hold the `memories` table, so the schema
/// batch above never runs for them. Each additive column is applied once, here.
///
/// Writes are remote-first, so this ALTER is a primary write and the local replica only
/// reflects it. That leaves one way to fail: the primary is unreachable, or it already carries
/// the column while this replica is too stale to show it. Both want the same answer, so the
/// error says so rather than surfacing a bare connection failure from an ALTER nobody asked for.
async fn add_missing_columns(conn: &Connection) -> Result<(), Error> {
    for (column, ddl) in [(
        "title",
        "ALTER TABLE memories ADD COLUMN title TEXT NOT NULL DEFAULT ''",
    )] {
        let present = scalar(
            conn,
            &format!("SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = '{column}'"),
        )
        .await?;
        if present > 0 {
            continue;
        }
        conn.execute(ddl, params![]).await.map_err(|e| {
            Error::Store(format!(
                "this replica predates the {column} column and the primary did not add it ({e}); \
                 connect once and retry"
            ))
        })?;
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

/// The API duplicates fields in both casings ("Name" and "name"), which breaks serde's
/// alias-based structs with a duplicate-field error; pick fields out of a Value instead.
fn json_field<'a>(value: &'a serde_json::Value, upper: &str, lower: &str) -> Option<&'a str> {
    value
        .get(upper)
        .or_else(|| value.get(lower))
        .and_then(serde_json::Value::as_str)
}

fn db_from_value(value: &serde_json::Value) -> Option<TursoDb> {
    let name = json_field(value, "Name", "name")?.to_string();
    let url = json_field(value, "LibsqlUrl", "libsql_url")
        .map(str::to_string)
        .or_else(|| {
            json_field(value, "Hostname", "hostname").map(|hostname| format!("libsql://{hostname}"))
        })?;
    Some(TursoDb { name, url })
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

    /// Returns the readable databases plus the count of entries the API returned that
    /// could not be parsed; the caller surfaces that count instead of silently thinning
    /// the workspace list.
    pub async fn list_databases(
        &self,
        org: &str,
        token: &str,
    ) -> Result<(Vec<TursoDb>, usize), Error> {
        #[derive(serde::Deserialize)]
        struct ListResp {
            #[serde(rename = "databases", default)]
            databases: Vec<serde_json::Value>,
        }
        let body = self
            .request(
                self.client
                    .get(format!(
                        "https://api.turso.tech/v1/organizations/{org}/databases"
                    ))
                    .bearer_auth(token),
                "list databases",
            )
            .await?;
        let parsed: ListResp = serde_json::from_str(&body).map_err(store_err)?;
        let total = parsed.databases.len();
        let dbs: Vec<TursoDb> = parsed.databases.iter().filter_map(db_from_value).collect();
        let skipped = total - dbs.len();
        Ok((dbs, skipped))
    }

    /// The group every database operation targets: the org's first existing group, or a
    /// freshly created "default" group in the closest region when the org has none yet.
    pub async fn ensure_group(&self, org: &str, token: &str) -> Result<String, Error> {
        #[derive(serde::Deserialize)]
        struct GroupsResp {
            #[serde(rename = "groups", default)]
            groups: Vec<serde_json::Value>,
        }
        let body = self
            .request(
                self.client
                    .get(format!(
                        "https://api.turso.tech/v1/organizations/{org}/groups"
                    ))
                    .bearer_auth(token),
                "list groups",
            )
            .await?;
        let parsed: GroupsResp = serde_json::from_str(&body).map_err(store_err)?;
        if let Some(name) = parsed
            .groups
            .iter()
            .find_map(|group| json_field(group, "Name", "name"))
        {
            return Ok(name.to_string());
        }

        #[derive(serde::Deserialize)]
        struct Region {
            server: String,
        }
        let region: Region = self
            .client
            .get("https://region.turso.io")
            .send()
            .await
            .map_err(store_err)?
            .json()
            .await
            .map_err(store_err)?;
        self.request(
            self.client
                .post(format!(
                    "https://api.turso.tech/v1/organizations/{org}/groups"
                ))
                .bearer_auth(token)
                .json(&serde_json::json!({ "name": "default", "location": region.server })),
            "create group",
        )
        .await?;
        Ok("default".to_string())
    }

    /// Idempotent: a 409 for an already-existing database resolves to that database, so
    /// a crashed earlier attempt cannot strand a name forever.
    pub async fn create_database(
        &self,
        org: &str,
        token: &str,
        group: &str,
        name: &str,
    ) -> Result<TursoDb, Error> {
        self.create(org, token, group, name, None).await
    }

    /// `seed` names a database to branch from: the new database opens as a copy of it, and the
    /// source never sees the branch. Only the migration test passes one — nothing `syn` does at
    /// runtime creates a database from another.
    async fn create(
        &self,
        org: &str,
        token: &str,
        group: &str,
        name: &str,
        seed: Option<&str>,
    ) -> Result<TursoDb, Error> {
        #[derive(serde::Deserialize)]
        struct CreateResp {
            #[serde(rename = "database")]
            database: serde_json::Value,
        }
        let mut body = serde_json::json!({ "name": name, "group": group });
        if let Some(from) = seed {
            body["seed"] = serde_json::json!({ "type": "database", "name": from });
        }
        let resp = self
            .client
            .post(format!(
                "https://api.turso.tech/v1/organizations/{org}/databases"
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(store_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(store_err)?;
        if status == reqwest::StatusCode::CONFLICT {
            let (dbs, _) = self.list_databases(org, token).await?;
            return dbs.into_iter().find(|db| db.name == name).ok_or_else(|| {
                Error::Store(format!(
                    "turso reports database {name} exists but the listing does not show it"
                ))
            });
        }
        if !status.is_success() {
            return Err(Error::Store(format!(
                "turso create database failed ({status}): {body}"
            )));
        }
        let parsed: CreateResp = serde_json::from_str(&body).map_err(store_err)?;
        db_from_value(&parsed.database)
            .ok_or_else(|| Error::Store(format!("unreadable create response: {body}")))
    }

    /// A platform API token authorizes the management endpoints only; opening or syncing
    /// a replica needs a database JWT, minted for the group (it covers every database in
    /// the group).
    pub async fn mint_db_token(
        &self,
        org: &str,
        token: &str,
        group: &str,
    ) -> Result<String, Error> {
        #[derive(serde::Deserialize)]
        struct TokenResp {
            jwt: String,
        }
        let body = self
            .request(
                self.client
                    .post(format!(
                        "https://api.turso.tech/v1/organizations/{org}/groups/{group}/auth/tokens?authorization=full-access"
                    ))
                    .bearer_auth(token),
                "mint db token",
            )
            .await?;
        let parsed: TokenResp = serde_json::from_str(&body).map_err(store_err)?;
        Ok(parsed.jwt)
    }

    pub async fn delete_database(&self, org: &str, token: &str, name: &str) -> Result<(), Error> {
        self.request(
            self.client
                .delete(format!(
                    "https://api.turso.tech/v1/organizations/{org}/databases/{name}"
                ))
                .bearer_auth(token),
            "delete database",
        )
        .await?;
        Ok(())
    }

    async fn request(&self, request: reqwest::RequestBuilder, what: &str) -> Result<String, Error> {
        let resp = request.send().await.map_err(store_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(store_err)?;
        if !status.is_success() {
            return Err(Error::Store(format!(
                "turso {what} failed ({status}): {body}"
            )));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn local_sql_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        // A plain local libSQL DB (same engine, same Connection API, no replication) exercises
        // the identical Store SQL/schema path fully offline.
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let store = LibsqlStore::init(db, "test-model", 4).await.unwrap();
        let memory = Memory {
            id: MemoryId::parse("m_0000000000000000000000").unwrap(),
            content: "deploy staging offers".to_string(),
            title: String::new(),
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
        store.insert(&memory, &embedding).await.unwrap();
        let got = store.get(&memory.id).await.unwrap().unwrap();
        assert_eq!(got.content, memory.content);
        assert_eq!(got.tags, memory.tags);
        let hits = store
            .keyword_search("deploy", &ScopeFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits, vec![memory.id.clone()]);

        let unrelated = Memory {
            id: MemoryId::parse("m_0000000000000000000001").unwrap(),
            content: "gift cards and rewards ship behind their own offers".to_string(),
            ..memory.clone()
        };
        store.insert(&unrelated, &embedding).await.unwrap();
        let rows_needed_before_bm25_can_rank = 2..8;
        for n in rows_needed_before_bm25_can_rank {
            let filler = Memory {
                id: MemoryId::parse(&format!("m_{n:022}")).unwrap(),
                content: format!("unrelated note {n} about postgres, kafka and elixir"),
                ..memory.clone()
            };
            store.insert(&filler, &embedding).await.unwrap();
        }
        let hits = store
            .keyword_search("deploy staging offers", &ScopeFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            hits,
            vec![memory.id.clone()],
            "a row matching one term of three is noise, not a hit"
        );
    }

    /// The schema the live replicas were created with, frozen. It is spelled out rather than
    /// derived from `SCHEMA` because it must keep describing the past whatever `SCHEMA` becomes.
    const LEGACY_SCHEMA: &str = r#"
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

    /// Build a replica holding one memory, with neither the `title` column nor a title in the
    /// index, as the live replicas did. Both absences are asserted rather than assumed, so this
    /// cannot decay into a test that quietly proves nothing.
    async fn pre_title_replica(path: &Path) -> Database {
        let db = libsql::Builder::new_local(path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(LEGACY_SCHEMA).await.unwrap();
        conn.execute(
            "INSERT INTO memories (id, content, kind, scope, tags, pinned, importance, embedding, \
             created_at, updated_at) VALUES (?, ?, 'reference', 'workspace', '[]', 0, 1, ?, ?, ?)",
            params![
                "m_0000000000000000000000",
                "an old fact",
                Value::Blob(vec![0u8; 16]),
                "2026-08-07T00:00:00Z",
                "2026-08-07T00:00:00Z",
            ],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO meta (id, embedding_model, embedding_dim) VALUES (1, 'test-model', 4)",
            params![],
        )
        .await
        .unwrap();
        let title_columns = scalar(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'title'",
        )
        .await
        .unwrap();
        assert_eq!(
            title_columns, 0,
            "the fixture must predate the title column"
        );
        let indexed_title = scalar(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('memories_fts') WHERE name = 'title'",
        )
        .await
        .unwrap();
        assert_eq!(indexed_title, 0, "the fixture must predate the wider index");
        db
    }

    /// The live replicas were created before `title` existed, and `ensure_schema` skips the
    /// schema batch whenever `memories` is already there — so the additive column is the only
    /// thing standing between an old replica and a store that cannot read its own rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replica_predating_the_title_column_gains_it_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        let db = pre_title_replica(&path).await;

        let store = LibsqlStore::init(db, "test-model", 4).await.unwrap();
        let id = MemoryId::parse("m_0000000000000000000000").unwrap();
        let got = store.get(&id).await.unwrap().unwrap();
        assert_eq!(got.content, "an old fact");
        assert_eq!(got.title, "", "a pre-existing row has no title");

        let patch = EditRequest {
            title: Some("Old fact".to_string()),
            ..EditRequest::default()
        };
        let updated = store
            .update(&id, &patch, None, &Timestamp::new("2026-08-11T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(updated.title, "Old fact");

        // Opening again must not re-issue the ALTER: every daemon boot runs this path.
        drop(store);
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let reopened = LibsqlStore::init(db, "test-model", 4).await.unwrap();
        assert_eq!(reopened.get(&id).await.unwrap().unwrap().title, "Old fact");
    }

    /// Overwrite the interior of every file the replica reads, leaving each header intact, so
    /// they still open and still answer queries but the b-tree no longer holds — the shape the
    /// real damage took. The WAL is overwritten rather than deleted on purpose: a missing WAL
    /// reads as a clean replica, and a corrupt one is the case under test.
    fn damage_pages(path: &Path) {
        use std::io::{Seek, SeekFrom, Write};
        let mut damaged_any = false;
        for suffix in ["", "-wal"] {
            let file_path = sibling(path, suffix);
            let Ok(mut file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&file_path)
            else {
                continue;
            };
            let len = file.metadata().unwrap().len() as usize;
            let from = 4096.min(len / 2);
            if len <= from + 512 {
                continue;
            }
            file.seek(SeekFrom::Start(from as u64)).unwrap();
            file.write_all(&vec![0x5A; len - from]).unwrap();
            file.flush().unwrap();
            damaged_any = true;
        }
        assert!(damaged_any, "nothing to damage at {}", path.display());
    }

    async fn populated_replica(path: &Path) {
        let db = libsql::Builder::new_local(path).build().await.unwrap();
        let store = LibsqlStore::init(db, "test-model", 4).await.unwrap();
        for n in 0..40 {
            let memory = test_memory(&format!("m_{n:022}"), &format!("fact number {n}"));
            store
                .insert(&memory, &[0.1f32, 0.2, 0.3, 0.4])
                .await
                .unwrap();
        }
    }

    async fn verdict(path: &Path, existed: bool) -> Option<String> {
        let db = libsql::Builder::new_local(path).build().await.unwrap();
        corruption(&db, existed).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scan_names_the_damage_and_passes_a_sound_replica() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        populated_replica(&path).await;
        assert_eq!(
            verdict(&path, true).await,
            None,
            "a sound replica must pass"
        );

        damage_pages(&path);
        assert!(
            verdict(&path, true).await.is_some(),
            "a damaged replica must be reported, not served"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replica_that_lost_its_schema_is_damage_but_a_first_open_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        assert_eq!(
            verdict(&path, false).await,
            None,
            "a machine opening its first replica has nothing to rebuild"
        );
        assert_eq!(
            verdict(&path, true).await,
            Some("the replica has no memories table".to_string()),
            "an existing replica without a schema lost it and must be rebuilt"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_replica_is_set_aside_rather_than_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica.db");
        populated_replica(&path).await;
        std::fs::write(sibling(&path, "-info"), b"stale v2 metadata").unwrap();
        std::fs::write(sibling(&path, "-client_wal_index"), b"stale v1 offset").unwrap();
        damage_pages(&path);

        quarantine_replica_files(&path).unwrap();

        for suffix in ["", "-wal", "-shm", "-info", "-client_wal_index"] {
            assert!(
                !sibling(&path, suffix).exists(),
                "libsql would still find {suffix:?} and resume from it"
            );
        }
        let kept: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".corrupt-"))
            .collect();
        for stem in ["replica.db.corrupt-", "replica.db-info.corrupt-"] {
            assert!(
                kept.iter().any(|name| name.starts_with(stem)),
                "{stem} must survive for recovery, got {kept:?}"
            );
        }
        assert!(!path.exists(), "the next build must see no replica at all");
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

    /// End-to-end remote-first replication against real Turso Cloud. Skips when
    /// `SYNAPSE_TURSO_TEST_TOKEN` (a platform API token) is unset, so `--ignored` runs
    /// stay usable without credentials. Provisions one throwaway database and deletes
    /// it afterwards.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Turso Cloud credentials and network"]
    async fn remote_first_writes_against_turso_cloud() {
        let Some(token) = std::env::var("SYNAPSE_TURSO_TEST_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            eprintln!("skipping: SYNAPSE_TURSO_TEST_TOKEN is not set");
            return;
        };
        let org =
            std::env::var("SYNAPSE_TURSO_TEST_ORG").unwrap_or_else(|_| "benediktms".to_string());

        let platform = TursoPlatform::new();
        let group = platform.ensure_group(&org, &token).await.unwrap();
        let name = format!("synapse-test-{}", std::process::id());
        let db = platform
            .create_database(&org, &token, &group, &name)
            .await
            .unwrap();
        let db_token = platform.mint_db_token(&org, &token, &group).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let url = db.url.clone();
        let store = without_stranding(&platform, &org, &token, &name, async move {
            turso_scenario(&path, &url, &db_token).await
        })
        .await
        .unwrap();

        // Once the primary is gone, a rejected mutation must not be acknowledged or
        // appear in the local read replica.
        let rejected = test_memory("m_0000000000000000000002", "must not exist locally");
        assert!(
            store.insert(&rejected, &[0.0f32; 4]).await.is_err(),
            "an unavailable primary must reject the Store write"
        );
        assert!(
            store.get(&rejected.id).await.unwrap().is_none(),
            "a rejected write must not modify the local read replica"
        );
    }

    /// The title migration against real rows. A branch of a live database is a copy the
    /// source never sees, so this runs the ALTER over the same shape and volume of data the
    /// real replicas hold — the thing a local fixture cannot tell you — and then proves the
    /// column reached the primary rather than only the replica that wrote it.
    ///
    /// `SYNAPSE_TURSO_TEST_SOURCE_DB` names the database to branch, and its embedding meta
    /// must be described by `SYNAPSE_TURSO_TEST_MODEL` / `_DIM`, since `open` refuses a
    /// database whose meta disagrees with the runtime it is handed.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Turso Cloud credentials, a source database, and network"]
    async fn a_branch_of_a_live_database_survives_the_title_migration() {
        let Some(token) = std::env::var("SYNAPSE_TURSO_TEST_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            eprintln!("skipping: SYNAPSE_TURSO_TEST_TOKEN is not set");
            return;
        };
        let org =
            std::env::var("SYNAPSE_TURSO_TEST_ORG").unwrap_or_else(|_| "benediktms".to_string());
        let source =
            std::env::var("SYNAPSE_TURSO_TEST_SOURCE_DB").unwrap_or_else(|_| "work".to_string());
        let model = std::env::var("SYNAPSE_TURSO_TEST_MODEL")
            .unwrap_or_else(|_| "bge-small-en-v1.5".to_string());
        let dim: usize = std::env::var("SYNAPSE_TURSO_TEST_DIM")
            .unwrap_or_else(|_| "384".to_string())
            .parse()
            .expect("SYNAPSE_TURSO_TEST_DIM must be a number");

        let platform = TursoPlatform::new();
        let group = platform.ensure_group(&org, &token).await.unwrap();
        let name = format!("synapse-migration-{}", std::process::id());
        let db = platform
            .create(&org, &token, &group, &name, Some(&source))
            .await
            .unwrap();
        let db_token = platform.mint_db_token(&org, &token, &group).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let url = db.url.clone();
        without_stranding(&platform, &org, &token, &name, async move {
            migration_scenario(&path, &url, &db_token, &model, dim).await
        })
        .await
        .unwrap();
    }

    /// Delete the throwaway database whatever the scenario did, including panicking: a failed
    /// assertion must not strand a database that every later daemon boot then has to explain.
    async fn without_stranding<T: Send + 'static>(
        platform: &TursoPlatform,
        org: &str,
        token: &str,
        name: &str,
        scenario: impl std::future::Future<Output = T> + Send + 'static,
    ) -> T {
        let outcome = tokio::spawn(scenario).await;
        platform.delete_database(org, token, name).await.unwrap();
        match outcome {
            Ok(value) => value,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    async fn migration_scenario(
        dir: &std::path::Path,
        url: &str,
        token: &str,
        model: &str,
        dim: usize,
    ) -> Result<(), domain::Error> {
        let open = |name: &str| {
            LibsqlStore::open(
                dir.join(name),
                url.to_string(),
                token.to_string(),
                model,
                dim,
            )
        };

        // The branch predates the column, so opening it runs the ALTER and every existing
        // row must still read back — with an empty title and a short form derived from it.
        let store_a = open("a.db").await?;
        let before = store_a.list().await?;
        assert!(!before.is_empty(), "branch a database that holds memories");
        for memory in &before {
            assert_eq!(memory.title, "", "{} gained a title", memory.id);
            assert!(
                !domain::short_form(&memory.title, &memory.content).is_empty(),
                "{} derives an empty short form",
                memory.id
            );
        }

        // Setting a title is a normal write, so it has to survive the round trip.
        let id = before[0].id.clone();
        let patch = EditRequest {
            title: Some("A title written after the migration".to_string()),
            ..EditRequest::default()
        };
        store_a
            .update(&id, &patch, None, &Timestamp::new("2026-08-11T00:00:00Z"))
            .await?;
        store_a.sync().await?;

        // The migration is only real if it reached the primary. Reading the fresh replica's
        // schema straight after its first pull, before any schema work of our own, is the
        // proof: the column can only be there because the primary carries it.
        let db_b = Builder::new_remote_replica(dir.join("b.db"), url.into(), token.into())
            .read_your_writes(true)
            .build()
            .await
            .map_err(store_err)?;
        db_b.sync().await.map_err(store_err)?;
        let title_columns = scalar(
            &db_b.connect().map_err(store_err)?,
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'title'",
        )
        .await?;
        assert_eq!(title_columns, 1, "the migration did not reach the primary");

        let store_b = LibsqlStore::init(db_b, model, dim).await?;
        assert_eq!(
            store_b.get(&id).await?.map(|m| m.title),
            Some("A title written after the migration".to_string()),
            "the second replica did not pull the migrated column and title"
        );
        assert_eq!(store_b.list().await?.len(), before.len());
        Ok(())
    }

    async fn turso_scenario(
        dir: &std::path::Path,
        url: &str,
        token: &str,
    ) -> Result<LibsqlStore, domain::Error> {
        let open = |name: &str| {
            LibsqlStore::open(
                dir.join(name),
                url.to_string(),
                token.to_string(),
                "test-model",
                4,
            )
        };

        let store_a = open("a.db").await?;
        let memory = test_memory("m_0000000000000000000001", "turso replication smoke test");
        store_a.insert(&memory, &[0.1f32, 0.2, 0.3, 0.4]).await?;

        // No explicit sync on store_a: a second replica can pull the memory because the
        // insert committed on the primary before it returned.
        let store_b = open("b.db").await?;
        let got = store_b
            .get(&memory.id)
            .await?
            .expect("acknowledged write reached the primary");
        assert_eq!(got.content, memory.content);

        drop(store_b);
        damage_pages(&dir.join("b.db"));
        let rebuilt = open("b.db").await?;
        assert_eq!(
            rebuilt.get(&memory.id).await?.map(|m| m.content),
            Some(memory.content.clone()),
            "the rebuilt replica lost the memory"
        );
        assert_eq!(rebuilt.list().await?.len(), 1);

        Ok(store_a)
    }

    fn test_memory(id: &str, content: &str) -> Memory {
        Memory {
            id: MemoryId::parse(id).unwrap(),
            content: content.to_string(),
            title: String::new(),
            kind: MemoryKind::parse("reference").unwrap(),
            scope: Scope::parse("workspace").unwrap(),
            tags: vec![],
            pinned: false,
            importance: domain::Importance::from_rank(1),
            created_at: Timestamp::new("2026-08-07T00:00:00Z".to_string()),
            updated_at: Timestamp::new("2026-08-07T00:00:00Z".to_string()),
        }
    }
}
