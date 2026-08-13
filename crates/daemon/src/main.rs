use daemon::DaemonApp;
use daemon::config::{Config, config_path};
use daemon::{rpc, single_instance};
use daemon_client::state_dir;

/// How long the run loop lingers after a shutdown request, so the connection task that
/// serves it can flush its reply before the process goes away.
const REPLY_FLUSH_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Logs go to stderr: that is the stream the CLI's spawn captures into daemon.log.
        .with_writer(std::io::stderr)
        .init();

    let dir = match state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };
    std::fs::create_dir_all(&dir).expect("create state dir");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restrict state dir");
    }

    let args: Vec<String> = std::env::args().collect();
    if let Some(command) = args.get(1).map(String::as_str)
        && command != "serve"
    {
        std::process::exit(maintenance(command, &args[2..], &dir).await);
    }

    // Single instance: flock the lock file; a second daemon exits 0 immediately. The binding is
    // held for the process lifetime (dropping it would release the lock).
    let _lock = match single_instance::acquire(&dir) {
        Ok(lock) => lock,
        Err(_) => {
            tracing::warn!("another daemon is already running; exiting");
            std::process::exit(0);
        }
    };

    let config = match Config::load(&config_path(&dir)) {
        Ok(c) => c,
        Err(_) => {
            tracing::error!(
                "no daemon config at {}; run `syn setup` first",
                config_path(&dir).display()
            );
            std::process::exit(1);
        }
    };

    if config.auto_update() {
        daemon::update::spawn();
    }

    let app = match DaemonApp::boot(dir.clone(), config).await {
        Ok(app) => app,
        Err(e) => {
            tracing::error!("boot failed: {e}");
            std::process::exit(1);
        }
    };

    let socket_path = dir.join("daemon.sock");
    let listener = match rpc::bind_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot bind socket {}: {e}", socket_path.display());
            std::process::exit(1);
        }
    };

    tracing::info!(
        "daemon ready on {} ({} workspace(s))",
        socket_path.display(),
        app.workspace_count().await
    );

    // Warm sync in the background: walk in and the replicas are already fresh, without
    // delaying the socket.
    {
        use daemon::rpc::RpcHost;
        let warm = app.clone();
        tokio::spawn(async move {
            let _ = warm.sync_replicas(None).await;
        });
    }

    tokio::select! {
        () = rpc::serve(listener, app.clone()) => {}
        () = app.shutdown_requested() => {
            tokio::time::sleep(REPLY_FLUSH_GRACE).await;
            tracing::info!("shutdown requested; exiting");
        }
    }
}

/// Offline upkeep over the replica files. It takes the same lock the daemon holds, because a
/// conversion rewrites vectors the daemon would otherwise be serving mid-walk.
async fn maintenance(command: &str, args: &[String], dir: &std::path::Path) -> i32 {
    if !matches!(command, "reembed" | "fts-rebuild") {
        eprintln!(
            "error: unknown command {command:?}; \
             usage: synd [serve | reembed --model <name> | fts-rebuild]"
        );
        return 1;
    }
    let _lock = match single_instance::acquire(dir) {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("error: the daemon is running; stop it with `syn daemon stop` first");
            return 1;
        }
    };
    let config = match Config::load(&config_path(dir)) {
        Ok(c) => c,
        Err(_) => {
            eprintln!(
                "error: no daemon config at {}; run `syn setup` first",
                config_path(dir).display()
            );
            return 1;
        }
    };
    let result = match command {
        "reembed" => run_reembed(args, dir, &config).await,
        "fts-rebuild" => daemon::maintenance::fts_rebuild(dir, &config)
            .await
            .map(|rebuilt| format!("rebuilt the keyword index of {rebuilt} workspace(s)")),
        other => unreachable!("unknown command {other:?} is rejected before the lock"),
    };
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(message) => {
            eprintln!("error: {message}");
            1
        }
    }
}

async fn run_reembed(
    args: &[String],
    dir: &std::path::Path,
    config: &Config,
) -> Result<String, String> {
    let model = match args {
        [flag, model] if flag == "--model" => model.as_str(),
        _ => return Err("usage: synd reembed --model <name>".into()),
    };
    if model != adapters_fastembed::MODEL_NAME {
        return Err(format!(
            "unsupported model {model:?}; this binary embeds with {:?}",
            adapters_fastembed::MODEL_NAME
        ));
    }
    let embedder = adapters_fastembed::FastEmbedder::with_cache_dir(dir.join("models"))
        .map_err(|e| format!("embedder: {e}"))?;
    let report = daemon::maintenance::reembed(
        dir,
        config,
        adapters_fastembed::MODEL_NAME,
        adapters_fastembed::DIMENSION,
        &embedder,
    )
    .await?;
    Ok(format!(
        "converted {} workspace(s), skipped {} already on {}",
        report.converted,
        report.skipped,
        adapters_fastembed::MODEL_NAME
    ))
}
