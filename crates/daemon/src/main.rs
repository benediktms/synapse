use daemon::DaemonApp;
use daemon::config::{Config, config_path};
use daemon::{rpc, single_instance};
use daemon_client::state_dir;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let dir = state_dir();
    std::fs::create_dir_all(&dir).expect("create state dir");
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

    rpc::serve(listener, app).await;
}
