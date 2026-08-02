use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use adapters_fastembed::FastEmbedder;
use server::App;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        None | Some("serve") => serve().await,
        Some("reembed") => reembed(&args[2..]).await,
        Some("fts-rebuild") => fts_rebuild().await,
        Some(other) => Err(format!(
            "unknown command {other:?}; usage: synapse-server [serve | reembed --model <name> | fts-rebuild]"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("SYNAPSE_DATA_DIR").unwrap_or_else(|_| "data".into()))
}

async fn load_embedder() -> Result<Arc<FastEmbedder>, String> {
    let embedder = tokio::task::spawn_blocking(FastEmbedder::new)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    info!(model = adapters_fastembed::MODEL_NAME, "model loaded");
    Ok(Arc::new(embedder))
}

async fn serve() -> Result<(), String> {
    let token = std::env::var("SYNAPSE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or("SYNAPSE_TOKEN must be set to a non-empty bearer token")?;
    let bind = std::env::var("SYNAPSE_BIND").unwrap_or_else(|_| "127.0.0.1:8737".into());
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| format!("invalid SYNAPSE_BIND {bind:?}: {e}"))?;
    let allow_nonlocal = matches!(
        std::env::var("SYNAPSE_ALLOW_NONLOCAL").as_deref(),
        Ok("1") | Ok("true")
    );
    if !addr.ip().is_loopback() && !allow_nonlocal {
        return Err(format!(
            "refusing non-loopback bind {addr}; set SYNAPSE_ALLOW_NONLOCAL=1 to allow"
        ));
    }
    let embedder = load_embedder().await?;
    let app = App::boot(data_dir(), embedder).await?;
    if let Err(reason) = api::Backend::ready(&app) {
        warn!(%reason, "serving unready");
    }
    let router = api::router(app, &token);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;
    info!(%addr, "listening");
    axum::serve(listener, router)
        .await
        .map_err(|e| e.to_string())
}

async fn reembed(args: &[String]) -> Result<(), String> {
    let model = match args {
        [flag, model] if flag == "--model" => model.as_str(),
        _ => return Err("usage: synapse-server reembed --model <name>".into()),
    };
    if model != adapters_fastembed::MODEL_NAME {
        return Err(format!(
            "unsupported model {model:?}; this binary embeds with {:?}",
            adapters_fastembed::MODEL_NAME
        ));
    }
    let embedder = load_embedder().await?;
    let report = server::reembed(
        &data_dir(),
        adapters_fastembed::MODEL_NAME,
        adapters_fastembed::DIMENSION,
        &*embedder,
    )
    .await?;
    info!(
        converted = report.converted,
        skipped = report.skipped,
        "reembed complete"
    );
    Ok(())
}

async fn fts_rebuild() -> Result<(), String> {
    let rebuilt = server::fts_rebuild(&data_dir()).await?;
    info!(rebuilt, "fts rebuild complete");
    Ok(())
}
