mod config;
mod error;
mod handlers;
mod models;
mod storage;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use config::ServerConfig;
use std::sync::Arc;
use storage::FileServer;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig::from_env();

    info!("File server starting on {}", config.listen_addr);
    info!(
        "Configuration: MaxFileSize={}MB, FileExpiration={}h, CleanupInterval={}m, MaxDownloads={}, ImageMaxDownloads={}",
        config.max_file_size / (1024 * 1024),
        config.file_expiration.num_hours(),
        config.cleanup_interval.num_minutes(),
        if config.max_download_count > 0 {
            config.max_download_count.to_string()
        } else {
            "unlimited".to_string()
        },
        if config.image_max_download_count > 0 {
            config.image_max_download_count.to_string()
        } else {
            "unlimited".to_string()
        }
    );

    if config.api_key.is_some() {
        info!("API authentication enabled");
    }

    let server = Arc::new(FileServer::new(config.clone()).await?);

    server.initialize_storage().await?;

    if let Err(e) = server.startup_cleanup().await {
        warn!("Startup cleanup encountered errors: {}", e);
    }

    let cleanup_server = server.clone();
    tokio::spawn(async move {
        FileServer::cleanup_worker(cleanup_server).await;
    });

    let max_body_size = config.max_file_size.min(usize::MAX as u64) as usize;

    let app = Router::new()
        .route("/upload", post(handlers::handle_upload))
        .route("/files/{path}", get(handlers::handle_download))
        .route("/list", get(handlers::handle_list_files))
        .layer(DefaultBodyLimit::max(max_body_size))
        .layer(RequestBodyLimitLayer::new(max_body_size))
        .with_state(server);

    let addr: std::net::SocketAddr = config.listen_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
