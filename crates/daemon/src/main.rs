use daemon::DaemonApp;
use daemon::config::{Config, config_path};
use daemon::{rpc, single_instance};
use daemon_client::state_dir;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let dir = match state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    std::fs::create_dir_all(&dir).expect("create state dir");
    init_tracing(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restrict state dir");
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

    rpc::serve(listener, app).await;
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// Logs go to stderr: that is the stream the CLI's spawn captures into daemon.log.
#[cfg(unix)]
fn init_tracing(_dir: &std::path::Path) {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::io::stderr)
        .init();
}

/// Task Scheduler discards stdout/stderr, so on Windows the daemon appends to
/// daemon.log itself — the same file `syn daemon logs` reads on every platform.
#[cfg(windows)]
fn init_tracing(dir: &std::path::Path) {
    let path = daemon_client::log_path(dir);
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .init();
}
