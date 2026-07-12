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
use config::{MULTIPART_OVERHEAD_BYTES, ServerConfig, format_duration};
use std::{process::ExitCode, sync::Arc};
use storage::FileServer;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tmpfile: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig::from_env()?;

    info!(
        "Configuration: MaxFileSize={}MiB, FileExpiration={}, CleanupInterval={}, MaxDownloads={}, ImageMaxDownloads={}",
        config.max_file_size / (1024 * 1024),
        format_duration(config.file_expiration),
        format_duration(config.cleanup_interval),
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
    std::mem::drop(tokio::spawn(async move {
        FileServer::cleanup_worker(cleanup_server).await;
    }));

    let app = build_app(server);

    let addr: std::net::SocketAddr = config.listen_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

fn build_app(server: Arc<FileServer>) -> Router {
    let max_file_size = usize::try_from(server.config.max_file_size).unwrap_or(usize::MAX);
    let max_body_size = max_file_size.saturating_add(MULTIPART_OVERHEAD_BYTES);

    Router::new()
        .route("/upload", post(handlers::handle_upload))
        .route("/files/{path}", get(handlers::handle_download))
        .route("/list", get(handlers::handle_list_files))
        // Multipart headers and boundaries count toward the request size. File
        // content is still checked precisely while it is streamed to disk.
        .layer(DefaultBodyLimit::max(max_body_size))
        .with_state(server)
}

#[cfg(test)]
mod http_tests;
