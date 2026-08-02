use adapters_sqlite::SqliteStore;
use domain::{Error, Memory, MemoryId, MemoryKind, Scope, ScopeFilter, Store, Timestamp};
use tempfile::TempDir;

const MODEL: &str = "test-model";
const DIM: usize = 4;

async fn open(dir: &TempDir) -> SqliteStore {
    SqliteStore::open(dir.path().join("ws.db"), MODEL, DIM)
        .await
        .unwrap()
}

fn ts(minute: u32) -> Timestamp {
    Timestamp::new(format!("2026-01-01T00:{minute:02}:00Z"))
}

fn mem(id: &MemoryId, content: &str, scope: Scope) -> Memory {
    Memory {
        id: id.clone(),
        content: content.to_string(),
        kind: MemoryKind::Project,
        scope,
        tags: vec!["alpha".to_string(), "beta".to_string()],
        pinned: false,
        created_at: ts(1),
        updated_at: ts(1),
    }
}

fn vec4(seed: f32) -> Vec<f32> {
    vec![seed, -seed, seed + 0.5, 0.0]
}

fn all() -> ScopeFilter {
    ScopeFilter { project: None }
}

#[tokio::test]
async fn insert_get_roundtrip_preserves_all_fields() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    let mut memory = mem(
        &id,
        "argocd deploy runbook",
        Scope::Project("fresha/offers".into()),
    );
    memory.pinned = true;
    store.insert(&memory, &vec4(0.1)).await.unwrap();
    assert_eq!(store.get(&id).await.unwrap().unwrap(), memory);
    assert_eq!(store.get(&MemoryId::generate()).await.unwrap(), None);
    assert_eq!(store.list().await.unwrap(), vec![memory]);
}

#[tokio::test]
async fn insert_same_payload_is_noop_different_payload_conflicts() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    let memory = mem(&id, "fact", Scope::Workspace);
    store.insert(&memory, &vec4(0.1)).await.unwrap();
    store.insert(&memory, &vec4(0.9)).await.unwrap();
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(store.embeddings(&all()).await.unwrap()[0].1, vec4(0.1));

    let different = mem(&id, "other fact", Scope::Workspace);
    assert_eq!(
        store.insert(&different, &vec4(0.2)).await.unwrap_err(),
        Error::Conflict(id)
    );
}

#[tokio::test]
async fn update_persists_fields_and_optionally_embedding() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    let mut memory = mem(&id, "old content", Scope::Workspace);
    store.insert(&memory, &vec4(0.1)).await.unwrap();

    memory.pinned = true;
    memory.updated_at = ts(2);
    store.update(&memory, None).await.unwrap();
    assert_eq!(store.get(&id).await.unwrap().unwrap(), memory);
    assert_eq!(store.embeddings(&all()).await.unwrap()[0].1, vec4(0.1));

    memory.content = "new content".to_string();
    memory.updated_at = ts(3);
    store.update(&memory, Some(&vec4(0.7))).await.unwrap();
    assert_eq!(store.get(&id).await.unwrap().unwrap(), memory);
    assert_eq!(store.embeddings(&all()).await.unwrap()[0].1, vec4(0.7));

    let missing = mem(&MemoryId::generate(), "ghost", Scope::Workspace);
    assert_eq!(
        store.update(&missing, None).await.unwrap_err(),
        Error::NotFound(missing.id.clone())
    );
}

#[tokio::test]
async fn delete_reports_whether_row_existed() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    store
        .insert(&mem(&id, "fact", Scope::Workspace), &vec4(0.1))
        .await
        .unwrap();
    assert!(store.delete(&id).await.unwrap());
    assert!(!store.delete(&id).await.unwrap());
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn fts_stays_in_sync_through_save_edit_delete() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    let mut memory = mem(&id, "argocd deploy staging", Scope::Workspace);
    store.insert(&memory, &vec4(0.1)).await.unwrap();
    assert_eq!(
        store.keyword_search("argocd", &all(), 10).await.unwrap(),
        vec![id.clone()]
    );

    memory.content = "datadog dashboard verification".to_string();
    store.update(&memory, Some(&vec4(0.2))).await.unwrap();
    assert!(
        store
            .keyword_search("argocd", &all(), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.keyword_search("datadog", &all(), 10).await.unwrap(),
        vec![id.clone()]
    );

    store.delete(&id).await.unwrap();
    assert!(
        store
            .keyword_search("datadog", &all(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn keyword_search_accepts_code_shaped_queries() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    store
        .insert(
            &mem(
                &id,
                "use std::collections::HashMap for caching",
                Scope::Workspace,
            ),
            &vec4(0.1),
        )
        .await
        .unwrap();
    for query in [
        "std::collections::HashMap",
        "fn caching() -> Result<(), Error>",
        r#"the "HashMap" type"#,
        "--use=HashMap (caching)",
    ] {
        assert_eq!(
            store.keyword_search(query, &all(), 10).await.unwrap(),
            vec![id.clone()],
            "query {query:?} should match"
        );
    }
    assert!(
        store
            .keyword_search("-> ::", &all(), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .keyword_search("", &all(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn scope_filter_limits_embeddings_and_keyword_search() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let ws_id = MemoryId::generate();
    let offers_id = MemoryId::generate();
    let billing_id = MemoryId::generate();
    store
        .insert(
            &mem(&ws_id, "workspace deploy fact", Scope::Workspace),
            &vec4(0.1),
        )
        .await
        .unwrap();
    store
        .insert(
            &mem(
                &offers_id,
                "offers deploy fact",
                Scope::Project("fresha/offers".into()),
            ),
            &vec4(0.2),
        )
        .await
        .unwrap();
    store
        .insert(
            &mem(
                &billing_id,
                "billing deploy fact",
                Scope::Project("fresha/billing".into()),
            ),
            &vec4(0.3),
        )
        .await
        .unwrap();

    let offers = ScopeFilter {
        project: Some("fresha/offers".to_string()),
    };
    let mut embedded: Vec<MemoryId> = store
        .embeddings(&offers)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    embedded.sort();
    let mut expected = vec![ws_id.clone(), offers_id.clone()];
    expected.sort();
    assert_eq!(embedded, expected);

    let mut found = store.keyword_search("deploy", &offers, 10).await.unwrap();
    found.sort();
    assert_eq!(found, expected);

    let workspace_only: Vec<MemoryId> = store
        .embeddings(&all())
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(workspace_only, vec![ws_id.clone()]);
    assert_eq!(
        store.keyword_search("deploy", &all(), 10).await.unwrap(),
        vec![ws_id]
    );
}

#[tokio::test]
async fn wrong_dimension_embedding_is_rejected_on_write_and_read() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    let memory = mem(&id, "fact", Scope::Workspace);
    assert!(store.insert(&memory, &[1.0, 2.0]).await.is_err());
    assert!(store.list().await.unwrap().is_empty());

    store.insert(&memory, &vec4(0.1)).await.unwrap();
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("ws.db").display()))
        .await
        .unwrap();
    sqlx::query("UPDATE memories SET embedding = x'00'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.embeddings(&all()).await.is_err());
}

#[tokio::test]
async fn open_is_rerunnable_and_enforces_meta() {
    let dir = TempDir::new().unwrap();
    let id = MemoryId::generate();
    {
        let store = open(&dir).await;
        store
            .insert(&mem(&id, "fact", Scope::Workspace), &vec4(0.1))
            .await
            .unwrap();
    }
    let reopened = open(&dir).await;
    assert!(reopened.get(&id).await.unwrap().is_some());
    assert_eq!(
        reopened.embedding_meta().await.unwrap(),
        (MODEL.to_string(), DIM)
    );

    let path = dir.path().join("ws.db");
    assert!(SqliteStore::open(&path, "other-model", DIM).await.is_err());
    assert!(SqliteStore::open(&path, MODEL, DIM + 1).await.is_err());
}

#[tokio::test]
async fn fts_rebuild_repairs_desynced_index() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let id = MemoryId::generate();
    store
        .insert(
            &mem(&id, "argocd deploy staging", Scope::Workspace),
            &vec4(0.1),
        )
        .await
        .unwrap();

    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("ws.db").display()))
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories_fts(memories_fts, rowid, content) SELECT 'delete', rowid, content FROM memories")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .keyword_search("argocd", &all(), 10)
            .await
            .unwrap()
            .is_empty()
    );

    store.fts_rebuild().await.unwrap();
    assert_eq!(
        store.keyword_search("argocd", &all(), 10).await.unwrap(),
        vec![id]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writes_do_not_surface_sqlite_busy() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let mut handles = Vec::new();
    for task in 0..8 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            for n in 0..25 {
                let id = MemoryId::generate();
                let memory = mem(&id, &format!("fact {task}-{n}"), Scope::Workspace);
                store.insert(&memory, &vec4(0.1)).await.unwrap();
                if n % 5 == 0 {
                    store.delete(&id).await.unwrap();
                }
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    assert_eq!(store.list().await.unwrap().len(), 8 * 20);
}
