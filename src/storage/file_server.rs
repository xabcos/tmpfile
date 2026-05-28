use super::{AppResult, MetadataStore, memory_store::MemoryStore, redis_store::RedisStore};
use crate::config::{
    CLEANUP_BATCH_SIZE, FILE_WRITE_BUFFER_SIZE, STARTUP_CLEANUP_CONCURRENCY, STORAGE_SUBDIR_DEPTH,
    ServerConfig, TOKEN_TOTAL_LENGTH,
};
use crate::error::AppError;
use crate::models::FileMetadata;
use axum::extract::multipart::Field;
use chrono::Utc;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::interval;
use tracing::{error, info};

#[derive(Clone)]
pub struct FileServer {
    pub config: Arc<ServerConfig>,
    pub store: Arc<dyn MetadataStore>,
}

impl FileServer {
    pub async fn new(config: ServerConfig) -> AppResult<Self> {
        let store: Arc<dyn MetadataStore> = if let Some(redis_addr) = &config.redis_addr {
            let redis_store = RedisStore::new(redis_addr, config.redis_db).await?;
            info!("Using Redis storage backend at {}", redis_addr);
            Arc::new(redis_store)
        } else {
            info!("Using in-memory storage backend");
            Arc::new(MemoryStore::new())
        };

        Ok(Self {
            config: Arc::new(config),
            store,
        })
    }

    pub async fn initialize_storage(&self) -> AppResult<()> {
        fs::create_dir_all(&self.config.upload_dir)
            .await
            .map_err(|e| {
                AppError::InternalServerError(format!("Failed to create upload directory: {}", e))
            })?;

        Ok(())
    }

    pub async fn startup_cleanup(&self) -> AppResult<()> {
        info!("Starting cleanup of orphaned files...");
        let start_time = std::time::Instant::now();
        let mut cleaned_count = 0;
        let mut total_size = 0i64;
        let mut error_count = 0;

        let semaphore = Arc::new(Semaphore::new(STARTUP_CLEANUP_CONCURRENCY));
        let mut join_set = JoinSet::new();

        let mut entries = fs::read_dir(&self.config.upload_dir).await.map_err(|e| {
            AppError::InternalServerError(format!("Failed to read upload directory: {}", e))
        })?;

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            AppError::InternalServerError(format!("Failed to read directory entry: {}", e))
        })? {
            let path = entry.path();
            let entry_type = entry.file_type().await.map_err(|e| {
                AppError::InternalServerError(format!("Failed to read directory entry type: {}", e))
            })?;
            if entry_type.is_dir() {
                let mut subdir_entries = fs::read_dir(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to read subdirectory: {}", e))
                })?;

                while let Some(file_entry) = subdir_entries.next_entry().await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to read file entry: {}", e))
                })? {
                    let file_path = file_entry.path();
                    let file_type = file_entry.file_type().await.map_err(|e| {
                        AppError::InternalServerError(format!("Failed to read file type: {}", e))
                    })?;
                    if file_type.is_file() {
                        let permit = semaphore.clone().acquire_owned().await.map_err(|e| {
                            AppError::InternalServerError(format!(
                                "Cleanup semaphore closed: {}",
                                e
                            ))
                        })?;
                        let server = self.clone();
                        join_set.spawn(async move {
                            let _permit = permit;
                            server.check_and_clean_file(file_path).await
                        });
                    }
                }
            }
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok((cleaned, size))) => {
                    if cleaned {
                        cleaned_count += 1;
                        total_size += size;
                    }
                }
                Ok(Err(e)) => {
                    error_count += 1;
                    error!("Error cleaning file during startup cleanup: {}", e);
                }
                Err(e) => {
                    error_count += 1;
                    error!("Startup cleanup task panicked: {}", e);
                }
            }
        }

        let duration = start_time.elapsed();
        info!(
            "Startup cleanup completed in {:?}: removed {} orphaned files ({:.2} MB), {} errors",
            duration,
            cleaned_count,
            total_size as f64 / (1024.0 * 1024.0),
            error_count
        );

        Ok(())
    }

    async fn check_and_clean_file(&self, path: PathBuf) -> AppResult<(bool, i64)> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| AppError::BadRequest("Invalid filename".to_string()))?;

        let Some(token) = filename.get(..TOKEN_TOTAL_LENGTH) else {
            info!("Found unrecognized file: {:?}", path);
            return Ok((false, 0));
        };

        match self.store.load(token).await {
            Ok(metadata) => {
                let now = Utc::now();

                if now > metadata.expires_at {
                    info!(
                        "Found expired file: {:?} (expired at {})",
                        path, metadata.expires_at
                    );
                    let size = metadata.size;
                    fs::remove_file(&path).await.map_err(|e| {
                        AppError::InternalServerError(format!(
                            "Failed to remove expired file: {}",
                            e
                        ))
                    })?;
                    self.store.remove(token).await?;
                    return Ok((true, size));
                }

                if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit
                {
                    info!(
                        "Found file exceeding download limit: {:?} ({}/{})",
                        path, metadata.download_count, metadata.download_limit
                    );
                    let size = metadata.size;
                    fs::remove_file(&path).await.map_err(|e| {
                        AppError::InternalServerError(format!("Failed to remove file: {}", e))
                    })?;
                    self.store.remove(token).await?;
                    return Ok((true, size));
                }

                Ok((false, 0))
            }
            Err(AppError::NotFound(_)) => {
                info!("Found orphaned file (no metadata): {:?}", path);
                let metadata = fs::metadata(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to get file metadata: {}", e))
                })?;
                let size = metadata.len() as i64;
                fs::remove_file(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to remove orphaned file: {}", e))
                })?;
                Ok((true, size))
            }
            Err(err) => {
                error!("Failed to load metadata for {}: {}", token, err);
                Err(AppError::InternalServerError(format!(
                    "Failed to load metadata: {}",
                    err
                )))
            }
        }
    }

    pub fn get_storage_path(&self, token: &str) -> PathBuf {
        let subdir = token.get(..STORAGE_SUBDIR_DEPTH).unwrap_or(token);
        PathBuf::from(&self.config.upload_dir)
            .join(subdir)
            .join(token)
    }

    pub async fn save_file(
        &self,
        token: &str,
        mut field: Field<'_>,
        filename: &str,
    ) -> AppResult<FileMetadata> {
        let extension = PathBuf::from(filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| ext.to_owned());

        let mut full_path = self.get_storage_path(token);
        if let Some(ext) = &extension {
            full_path.set_extension(ext);
        }

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let file = File::create(&full_path).await?;
        let mut writer = BufWriter::with_capacity(FILE_WRITE_BUFFER_SIZE, file);
        let mut total_size: u64 = 0;

        loop {
            let maybe_chunk = field.chunk().await.map_err(AppError::from)?;

            let Some(chunk) = maybe_chunk else {
                break;
            };
            let chunk_bytes = chunk.as_ref();

            total_size = total_size
                .checked_add(chunk_bytes.len() as u64)
                .ok_or_else(|| AppError::BadRequest("file size overflow".to_string()))?;

            if total_size > self.config.max_file_size {
                drop(writer);
                if let Err(e) = fs::remove_file(&full_path).await
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    error!(
                        "Failed to remove oversized temp file {}: {}",
                        full_path.display(),
                        e
                    );
                }
                return Err(AppError::PayloadTooLarge);
            }

            writer.write_all(chunk_bytes).await?;
        }

        writer.flush().await?;

        let now = Utc::now();
        let extension_with_dot = extension
            .as_ref()
            .map(|ext| format!(".{}", ext))
            .unwrap_or_default();

        let is_image = extension
            .as_deref()
            .map(is_image_extension)
            .unwrap_or(false);

        Ok(FileMetadata {
            original_name: filename.to_string(),
            storage_path: full_path.to_string_lossy().to_string(),
            size: total_size as i64,
            extension: extension_with_dot,
            uploaded_at: now,
            expires_at: now + self.config.file_expiration,
            download_count: 0,
            download_limit: if is_image {
                self.config.image_max_download_count
            } else {
                self.config.max_download_count
            },
        })
    }

    pub async fn remove_file(&self, token: &str, metadata: &FileMetadata) {
        if let Err(e) = fs::remove_file(&metadata.storage_path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            error!("Failed to remove file {}: {}", metadata.storage_path, e);
        }

        if let Err(e) = self.store.remove(token).await {
            error!("Failed to remove metadata for token {}: {}", token, e);
        }

        info!("Removed: {} -> {}", metadata.original_name, token);
    }

    pub async fn cleanup_worker(server: Arc<FileServer>) {
        let cleanup_interval = server.config.cleanup_interval.to_std().unwrap_or_else(|e| {
            error!(
                "Invalid cleanup interval {:?}: {}. Falling back to 60s.",
                server.config.cleanup_interval, e
            );
            StdDuration::from_secs(60)
        });

        let mut interval = interval(std::cmp::max(cleanup_interval, StdDuration::from_secs(1)));

        loop {
            interval.tick().await;
            server.perform_cleanup().await;
        }
    }

    async fn perform_cleanup(&self) {
        loop {
            match self.store.list_expired(CLEANUP_BATCH_SIZE).await {
                Ok((tokens, metadata_list)) => {
                    if tokens.is_empty() {
                        break;
                    }

                    let count = tokens.len();

                    let mut join_set = JoinSet::new();
                    for (token, metadata) in tokens.into_iter().zip(metadata_list.into_iter()) {
                        let server = self.clone();
                        join_set.spawn(async move {
                            server.remove_file(&token, &metadata).await;
                        });
                    }

                    while let Some(result) = join_set.join_next().await {
                        if let Err(e) = result {
                            error!("Cleanup task panicked: {}", e);
                        }
                    }

                    info!("Cleaned up {} expired files", count);

                    if count < CLEANUP_BATCH_SIZE {
                        break;
                    }
                }
                Err(e) => {
                    error!("Cleanup error: {}", e);
                    break;
                }
            }
        }
    }
}

fn is_image_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif" | "svg" | "ico" | "bmp"
    )
}
