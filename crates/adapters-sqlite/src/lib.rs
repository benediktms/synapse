use std::path::Path;
use std::time::Duration;

use domain::{
    EditRequest, Embedder, Error, Memory, MemoryId, MemoryKind, Scope, ScopeFilter, Store,
    Timestamp,
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

const MAX_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
    dim: usize,
}

impl SqliteStore {
    pub async fn open(
        path: impl AsRef<Path>,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Result<Self, Error> {
        let pool = connect(path, true).await?;
        let dim = i64::try_from(embedding_dim).map_err(store_err)?;
        if !meta_exists(&pool).await? {
            let memories = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM memories"#)
                .fetch_one(&pool)
                .await
                .map_err(store_err)?;
            if memories > 0 {
                return Err(Error::Store(format!(
                    "{memories} memories but no embedding meta row: their vectors cannot be \
                     attributed to a model and will not be re-embedded — restore the database \
                     from a backup, or re-import its export into a fresh one"
                )));
            }
            sqlx::query!(
                "INSERT INTO meta (id, embedding_model, embedding_dim) VALUES (1, ?, ?) \
                 ON CONFLICT (id) DO NOTHING",
                embedding_model,
                dim
            )
            .execute(&pool)
            .await
            .map_err(store_err)?;
        }
        let meta = sqlx::query!("SELECT embedding_model, embedding_dim FROM meta WHERE id = 1")
            .fetch_one(&pool)
            .await
            .map_err(store_err)?;
        if meta.embedding_model != embedding_model || meta.embedding_dim != dim {
            return Err(Error::Store(format!(
                "embedding meta mismatch: db has {} ({} dims), runtime uses {} ({} dims)",
                meta.embedding_model, meta.embedding_dim, embedding_model, embedding_dim
            )));
        }
        Ok(Self {
            pool,
            dim: embedding_dim,
        })
    }

    pub async fn open_maintenance(path: impl AsRef<Path>) -> Result<Self, Error> {
        let pool = connect(path, false).await?;
        let meta = sqlx::query!("SELECT embedding_model, embedding_dim FROM meta WHERE id = 1")
            .fetch_optional(&pool)
            .await
            .map_err(store_err)?
            .ok_or_else(|| {
                Error::Store("no embedding meta; boot the server once to initialize".into())
            })?;
        let dim = usize::try_from(meta.embedding_dim).map_err(store_err)?;
        Ok(Self { pool, dim })
    }

    pub async fn reembed<E: Embedder>(
        self,
        embedding_model: &str,
        embedding_dim: usize,
        embedder: &E,
    ) -> Result<usize, Error> {
        let dim = i64::try_from(embedding_dim).map_err(store_err)?;
        let mut tx = self.pool.begin().await.map_err(store_err)?;
        let rows = sqlx::query!("SELECT id, content FROM memories")
            .fetch_all(&mut *tx)
            .await
            .map_err(store_err)?;
        for row in &rows {
            let embedding = embedder.embed(&row.content).await?;
            let blob = encode_embedding(&embedding, embedding_dim)?;
            sqlx::query!(
                "UPDATE memories SET embedding = ? WHERE id = ?",
                blob,
                row.id
            )
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;
        }
        sqlx::query!(
            "UPDATE meta SET embedding_model = ?, embedding_dim = ? WHERE id = 1",
            embedding_model,
            dim
        )
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;
        Ok(rows.len())
    }

    pub async fn embedding_meta(&self) -> Result<(String, usize), Error> {
        let meta = sqlx::query!("SELECT embedding_model, embedding_dim FROM meta WHERE id = 1")
            .fetch_one(&self.pool)
            .await
            .map_err(store_err)?;
        Ok((
            meta.embedding_model,
            usize::try_from(meta.embedding_dim).map_err(store_err)?,
        ))
    }

    pub async fn fts_rebuild(&self) -> Result<(), Error> {
        sqlx::query("INSERT INTO memories_fts(memories_fts) VALUES ('rebuild')")
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}

impl Store for SqliteStore {
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>, Error> {
        let id = id.as_str();
        sqlx::query_as!(
            MemoryRow,
            "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at \
             FROM memories WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?
        .map(Memory::try_from)
        .transpose()
    }

    async fn get_with_embedding(&self, id: &MemoryId) -> Result<Option<(Memory, Vec<f32>)>, Error> {
        let key = id.as_str();
        let row = sqlx::query!(
            "SELECT id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at \
             FROM memories WHERE id = ?",
            key
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let embedding = decode_embedding(&row.embedding, self.dim)?;
        let memory = Memory::try_from(MemoryRow {
            id: row.id,
            content: row.content,
            kind: row.kind,
            scope: row.scope,
            tags: row.tags,
            pinned: row.pinned,
            importance: row.importance,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })?;
        Ok(Some((memory, embedding)))
    }

    async fn insert(&self, memory: &Memory, embedding: &[f32]) -> Result<(), Error> {
        let blob = encode_embedding(embedding, self.dim)?;
        let id = memory.id.as_str();
        let kind = memory.kind.as_str();
        let scope = memory.scope.as_str();
        let tags = serde_json::to_string(&memory.tags).map_err(store_err)?;
        let pinned = memory.pinned as i64;
        let importance = i64::from(memory.importance.rank());
        let created_at = memory.created_at.as_str();
        let updated_at = memory.updated_at.as_str();
        let result = sqlx::query!(
            "INSERT INTO memories (id, content, kind, scope, tags, pinned, importance, embedding, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
            id,
            memory.content,
            kind,
            scope,
            tags,
            pinned,
            importance,
            blob,
            created_at,
            updated_at
        )
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        if result.rows_affected() == 1 {
            return Ok(());
        }
        let existing = self
            .get(&memory.id)
            .await?
            .ok_or_else(|| Error::Store(format!("memory {id} vanished during insert")))?;
        let same_payload = existing.content == memory.content
            && existing.kind == memory.kind
            && existing.scope == memory.scope
            && existing.tags == memory.tags;
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
        let content = patch.content.as_deref();
        let tags = patch
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(store_err)?;
        let pinned = patch.pinned.map(i64::from);
        let importance = patch.importance.map(|tier| i64::from(tier.rank()));
        let blob = embedding
            .map(|embedding| encode_embedding(embedding, self.dim))
            .transpose()?;
        let updated_at = now.as_str();
        let key = id.as_str();
        sqlx::query_as!(
            MemoryRow,
            r#"UPDATE memories SET content = COALESCE(?, content), tags = COALESCE(?, tags),
                   pinned = COALESCE(?, pinned), importance = COALESCE(?, importance),
                   embedding = COALESCE(?, embedding), updated_at = ?
               WHERE id = ?
               RETURNING id AS "id!", content AS "content!", kind AS "kind!", scope AS "scope!",
                   tags AS "tags!", pinned AS "pinned!", importance AS "importance!",
                   created_at AS "created_at!", updated_at AS "updated_at!""#,
            content,
            tags,
            pinned,
            importance,
            blob,
            updated_at,
            key
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::NotFound(id.clone()))
        .and_then(Memory::try_from)
    }

    async fn delete(&self, id: &MemoryId) -> Result<bool, Error> {
        let id = id.as_str();
        let result = sqlx::query!("DELETE FROM memories WHERE id = ?", id)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn list(&self) -> Result<Vec<Memory>, Error> {
        sqlx::query_as!(
            MemoryRow,
            "SELECT id, content, kind, scope, tags, pinned, importance, created_at, updated_at FROM memories"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?
        .into_iter()
        .map(Memory::try_from)
        .collect()
    }

    async fn embeddings(&self, filter: &ScopeFilter) -> Result<Vec<(MemoryId, Vec<f32>)>, Error> {
        let project = filter.project.as_deref().unwrap_or("");
        let rows = sqlx::query!(
            "SELECT id, embedding FROM memories WHERE scope = 'workspace' OR scope = ?",
            project
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    MemoryId::parse(&row.id)?,
                    decode_embedding(&row.embedding, self.dim)?,
                ))
            })
            .collect()
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
        let project = filter.project.as_deref().unwrap_or("");
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query!(
            r#"SELECT m.id AS "id!" FROM memories_fts
               JOIN memories m ON m.rowid = memories_fts.rowid
               WHERE memories_fts MATCH ? AND (m.scope = 'workspace' OR m.scope = ?)
               ORDER BY memories_fts.rank
               LIMIT ?"#,
            fts_query,
            project,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.into_iter()
            .map(|row| MemoryId::parse(&row.id))
            .collect()
    }
}

async fn meta_exists(pool: &SqlitePool) -> Result<bool, Error> {
    sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM meta WHERE id = 1"#)
        .fetch_one(pool)
        .await
        .map(|count| count > 0)
        .map_err(store_err)
}

async fn connect(path: impl AsRef<Path>, create_if_missing: bool) -> Result<SqlitePool, Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create_if_missing)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(BUSY_TIMEOUT)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await
        .map_err(store_err)?;
    MIGRATOR.run(&pool).await.map_err(store_err)?;
    Ok(pool)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_quotes_each_whitespace_token() {
        assert_eq!(
            escape_fts_query("deploy staging offers").unwrap(),
            r#""deploy" OR "staging" OR "offers""#
        );
    }

    #[test]
    fn escape_handles_code_shaped_input() {
        assert_eq!(
            escape_fts_query("std::collections::HashMap").unwrap(),
            r#""std::collections::HashMap""#
        );
        assert_eq!(
            escape_fts_query("fn foo() -> Result<T, E>").unwrap(),
            r#""fn" OR "foo()" OR "Result<T," OR "E>""#
        );
        assert_eq!(
            escape_fts_query("--flag=value (parens)").unwrap(),
            r#""--flag=value" OR "(parens)""#
        );
    }

    #[test]
    fn escape_doubles_embedded_quotes() {
        assert_eq!(
            escape_fts_query(r#"say "hello world""#).unwrap(),
            r#""say" OR """hello" OR "world""""#
        );
    }

    #[test]
    fn escape_drops_tokens_without_alphanumerics() {
        assert_eq!(escape_fts_query("-> :: foo").unwrap(), r#""foo""#);
        assert_eq!(escape_fts_query("-> :: -"), None);
        assert_eq!(escape_fts_query("   "), None);
        assert_eq!(escape_fts_query(""), None);
    }

    #[test]
    fn embedding_roundtrips_le_f32() {
        let original = vec![1.0f32, -0.5, 0.0, 3.25];
        let blob = encode_embedding(&original, 4).unwrap();
        assert_eq!(blob.len(), 16);
        assert_eq!(&blob[..4], &1.0f32.to_le_bytes());
        assert_eq!(decode_embedding(&blob, 4).unwrap(), original);
    }

    #[test]
    fn embedding_dim_mismatch_is_error() {
        assert!(encode_embedding(&[1.0, 2.0], 4).is_err());
        assert!(decode_embedding(&[0u8; 8], 4).is_err());
        assert!(decode_embedding(&[0u8; 15], 4).is_err());
    }
}
