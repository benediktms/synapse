use adapters_fastembed::FastEmbedder;
use adapters_libsql::LibsqlStore;
use clap::{Parser, ValueEnum};
use domain::{
    Embedder, Importance, MIN_VECTOR_SIMILARITY, Memory, MemoryId, MemoryKind, RecallRequest,
    Scope, ScopeFilter, Store, Timestamp, Workspace, embed_text, recall,
};
use std::{
    collections::HashSet,
    error::Error,
    path::{Path, PathBuf},
};

const STAMP: &str = "2026-01-01T00:00:00Z";
const WORKSPACE: &str = "bench";

#[derive(Parser)]
#[command(about = "Score recall against a labelled synthetic corpus")]
struct Args {
    /// Corpus database holding `docs(rowid, content, labels)` and `queries(q, cls, topic)`.
    #[arg(long)]
    corpus: PathBuf,

    /// Seeded store. Reused across runs; embedding it again is the slow part.
    #[arg(long, default_value = "target/bench-store.db")]
    store: PathBuf,

    /// Discard the seeded store and embed the corpus again.
    #[arg(long)]
    reseed: bool,

    /// How many hits each query asks for.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Which retrieval lane to score. `both` is the fused path a real recall takes.
    #[arg(long, value_enum, default_value_t = Lane::Both)]
    lane: Lane,

    /// Print a line per query.
    #[arg(long)]
    verbose: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Lane {
    Both,
    Vector,
    Keyword,
}

impl Lane {
    fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Vector => "vector",
            Self::Keyword => "keyword",
        }
    }
}

struct Doc {
    id: MemoryId,
    content: String,
    topics: Vec<String>,
}

struct Query {
    text: String,
    expects_hits: bool,
    topic: String,
}

struct Corpus {
    docs: Vec<Doc>,
    queries: Vec<Query>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let corpus = read_corpus(&args.corpus).await?;

    if args.reseed && args.store.exists() {
        std::fs::remove_file(&args.store)?;
    }
    let fresh = !args.store.exists();

    let embedder = FastEmbedder::new()?;
    let store = open_store(&args.store, &embedder).await?;

    if fresh {
        seed(&store, &embedder, &corpus.docs).await?;
    } else {
        let held = store.list().await?.len();
        if held != corpus.docs.len() {
            return Err(format!(
                "{} holds {held} memories but the corpus has {}; pass --reseed",
                args.store.display(),
                corpus.docs.len()
            )
            .into());
        }
    }

    score(&args, &store, &embedder, &corpus).await
}

async fn read_corpus(path: &Path) -> Result<Corpus, Box<dyn Error>> {
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;

    let mut docs = Vec::new();
    let mut rows = conn
        .query("SELECT rowid, content, labels FROM docs ORDER BY rowid", ())
        .await?;
    while let Some(row) = rows.next().await? {
        let rowid: i64 = row.get(0)?;
        let content: String = row.get(1)?;
        let labels: String = row.get(2)?;
        docs.push(Doc {
            id: MemoryId::parse(&format!("m_{rowid:022}"))?,
            content,
            topics: serde_json::from_str(&labels)?,
        });
    }

    let mut queries = Vec::new();
    let mut rows = conn.query("SELECT q, cls, topic FROM queries", ()).await?;
    while let Some(row) = rows.next().await? {
        let text: String = row.get(0)?;
        let cls: String = row.get(1)?;
        let topic: String = row.get(2)?;
        queries.push(Query {
            text,
            expects_hits: cls == "yes",
            topic,
        });
    }

    Ok(Corpus { docs, queries })
}

async fn open_store(path: &Path, embedder: &FastEmbedder) -> Result<LibsqlStore, Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = libsql::Builder::new_local(path).build().await?;
    Ok(LibsqlStore::init(db, embedder.model_name(), embedder.dimension()).await?)
}

async fn seed(
    store: &LibsqlStore,
    embedder: &FastEmbedder,
    docs: &[Doc],
) -> Result<(), Box<dyn Error>> {
    for (done, doc) in docs.iter().enumerate() {
        let memory = Memory {
            id: doc.id.clone(),
            content: doc.content.clone(),
            title: String::new(),
            kind: MemoryKind::Reference,
            scope: Scope::Workspace,
            tags: doc.topics.clone(),
            pinned: false,
            importance: Importance::DEFAULT,
            created_at: Timestamp::new(STAMP),
            updated_at: Timestamp::new(STAMP),
        };
        let embedding = embedder
            .embed(&embed_text(&memory.title, &memory.content))
            .await?;
        store.insert(&memory, &embedding).await?;
        if (done + 1) % 100 == 0 {
            eprintln!("seeded {}/{}", done + 1, docs.len());
        }
    }
    Ok(())
}

async fn score(
    args: &Args,
    store: &LibsqlStore,
    embedder: &FastEmbedder,
    corpus: &Corpus,
) -> Result<(), Box<dyn Error>> {
    let ws = Workspace::new(WORKSPACE)?;
    let mut precisions = Vec::new();
    let mut recalls = Vec::new();
    let mut noise_total = 0;
    let mut noise_fired = 0;

    for query in &corpus.queries {
        let hits = search(args.lane, store, embedder, &ws, &query.text, args.limit).await?;

        if !query.expects_hits {
            noise_total += 1;
            if !hits.is_empty() {
                noise_fired += 1;
            }
            if args.verbose {
                println!("{:<12} {:>2} hits  {}", "[noise]", hits.len(), query.text);
            }
            continue;
        }

        let relevant = relevant_ids(&corpus.docs, &query.topic);
        let found = hits.iter().filter(|id| relevant.contains(id)).count();
        let precision = ratio(found, hits.len());
        let reachable_relevant = relevant.len().min(args.limit);
        let recall_at_k = if reachable_relevant == 0 {
            1.0
        } else {
            ratio(found, reachable_relevant)
        };
        precisions.push(precision);
        recalls.push(recall_at_k);
        if args.verbose {
            println!(
                "{:<12} P={precision:.2} R={recall_at_k:.2}  {}",
                query.topic, query.text
            );
        }
    }

    let p = mean(&precisions);
    let r = mean(&recalls);
    let f1 = if p + r > 0.0 {
        2.0 * p * r / (p + r)
    } else {
        0.0
    };
    let k = args.limit;

    println!(
        "\nlane={} docs={} scored={} noise={noise_total}",
        args.lane.as_str(),
        corpus.docs.len(),
        precisions.len()
    );
    println!("P@{k}={p:.3}  R@{k}={r:.3}  F1={f1:.3}");
    println!("noise false positives: {noise_fired}/{noise_total}");
    Ok(())
}

/// The isolated lanes mirror what `hybrid_search` feeds into the fusion, at `limit` depth rather
/// than the domain's private `CANDIDATE_DEPTH`: depth changes how many a lane returns, never
/// whether it returns anything, which is what the noise queries ask.
async fn search(
    lane: Lane,
    store: &LibsqlStore,
    embedder: &FastEmbedder,
    ws: &Workspace,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryId>, Box<dyn Error>> {
    let filter = ScopeFilter { project: None };
    match lane {
        Lane::Both => {
            let req = RecallRequest {
                query: query.to_string(),
                project: None,
                limit,
                links_in_scope: false,
            };
            Ok(recall(embedder, (ws, store), None, &req)
                .await?
                .into_iter()
                .map(|hit| hit.memory.id)
                .collect())
        }
        Lane::Vector => {
            let query_vec = embedder.embed(query).await?;
            Ok(store
                .vector_search(&query_vec, &filter, limit)
                .await?
                .into_iter()
                .filter(|hit| hit.similarity >= MIN_VECTOR_SIMILARITY)
                .map(|hit| hit.id)
                .collect())
        }
        Lane::Keyword => Ok(store
            .keyword_search(query, &filter, limit)
            .await?
            .into_iter()
            .map(|hit| hit.id)
            .collect()),
    }
}

fn relevant_ids(docs: &[Doc], topic: &str) -> HashSet<MemoryId> {
    docs.iter()
        .filter(|doc| doc.topics.iter().any(|held| held == topic))
        .map(|doc| doc.id.clone())
        .collect()
}

fn ratio(found: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        found as f64 / total as f64
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}
