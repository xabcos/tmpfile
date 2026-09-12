use super::{
    AppResult, MetadataStore, TokenGenerator, is_valid_extension, memory_store::MemoryStore,
    redis_store::RedisStore,
};
use crate::config::{
    CLEANUP_BATCH_SIZE, FILE_WRITE_BUFFER_SIZE, STARTUP_CLEANUP_CONCURRENCY, STORAGE_SUBDIR_DEPTH,
    ServerConfig,
};
use crate::error::AppError;
use crate::models::FileMetadata;
use axum::body::Body;
use axum::extract::multipart::Field;
use chrono::Utc;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio::time::interval;
use tracing::{error, info};

const PARTIAL_UPLOAD_SUFFIX: &str = ".upload";
const VIDEO_EXTENSION: &str = "mp4";
const VIDEO_FILENAME: &str = "grok-video.mp4";

#[derive(Clone)]
pub struct FileServer {
    pub config: Arc<ServerConfig>,
    pub(crate) store: Arc<MetadataStore>,
}

struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    const fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn persist(mut self) {
        self.path = None;
    }

    async fn remove(mut self) {
        if let Some(path) = self.path.take()
            && let Err(err) = fs::remove_file(&path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            error!("Failed to remove pending file {}: {}", path.display(), err);
        }
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            std::mem::drop(runtime.spawn(async move {
                if let Err(err) = fs::remove_file(&path).await
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    error!(
                        "Failed to remove unfinished file {}: {}",
                        path.display(),
                        err
                    );
                }
            }));
        } else if let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            error!(
                "Failed to remove unfinished file {}: {}",
                path.display(),
                err
            );
        }
    }
}

impl FileServer {
    pub async fn new(config: ServerConfig) -> AppResult<Self> {
        let store = if let Some(redis_addr) = &config.redis_addr {
            let redis_store = RedisStore::new(redis_addr, config.redis_db).await?;
            info!("Using Redis storage backend at {}", redis_addr);
            MetadataStore::Redis(redis_store)
        } else {
            info!("Using in-memory storage backend");
            MetadataStore::Memory(MemoryStore::new())
        };

        Ok(Self {
            config: Arc::new(config),
            store: Arc::new(store),
        })
    }

    pub async fn initialize_storage(&self) -> AppResult<()> {
        fs::create_dir_all(&self.config.upload_dir)
            .await
            .map_err(|e| {
                AppError::InternalServerError(format!("Failed to create upload directory: {e}"))
            })?;

        Ok(())
    }

    pub async fn startup_cleanup(&self) -> AppResult<()> {
        info!("Starting cleanup of orphaned files...");
        let start_time = std::time::Instant::now();
        let mut cleaned_count = 0;
        let mut total_size = 0i64;
        let mut error_count = 0;

        let mut join_set = JoinSet::new();

        let mut entries = fs::read_dir(&self.config.upload_dir).await.map_err(|e| {
            AppError::InternalServerError(format!("Failed to read upload directory: {e}"))
        })?;

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            AppError::InternalServerError(format!("Failed to read directory entry: {e}"))
        })? {
            let path = entry.path();
            let entry_type = entry.file_type().await.map_err(|e| {
                AppError::InternalServerError(format!("Failed to read directory entry type: {e}"))
            })?;
            if entry_type.is_dir() {
                let mut subdir_entries = fs::read_dir(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to read subdirectory: {e}"))
                })?;

                while let Some(file_entry) = subdir_entries.next_entry().await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to read file entry: {e}"))
                })? {
                    let file_path = file_entry.path();
                    let file_type = match file_entry.file_type().await {
                        Ok(file_type) => file_type,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => {
                            return Err(AppError::InternalServerError(format!(
                                "Failed to read file type: {err}"
                            )));
                        }
                    };
                    if file_type.is_file() {
                        let server = self.clone();
                        join_set.spawn(async move { server.check_and_clean_file(file_path).await });

                        if join_set.len() >= STARTUP_CLEANUP_CONCURRENCY
                            && let Some(result) = join_set.join_next().await
                        {
                            record_cleanup_result(
                                result,
                                &mut cleaned_count,
                                &mut total_size,
                                &mut error_count,
                            );
                        }
                    }
                }
            }
        }

        while let Some(result) = join_set.join_next().await {
            record_cleanup_result(
                result,
                &mut cleaned_count,
                &mut total_size,
                &mut error_count,
            );
        }

        let duration = start_time.elapsed();
        info!(
            "Startup cleanup completed in {:?}: removed {} orphaned files ({} bytes), {} errors",
            duration, cleaned_count, total_size, error_count
        );

        Ok(())
    }

    async fn check_and_clean_file(&self, path: PathBuf) -> AppResult<(bool, i64)> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| AppError::BadRequest("Invalid filename".to_string()))?;

        if auxiliary_token(filename, PARTIAL_UPLOAD_SUFFIX).is_some() {
            return remove_startup_artifact(&path).await;
        }

        let Some(token) = TokenGenerator::parse_path(filename) else {
            info!("Found unrecognized file: {:?}", path);
            return remove_startup_artifact(&path).await;
        };

        match self.store.load(token).await {
            Ok(metadata) => {
                let now = Utc::now();

                if self.get_file_path(token, &metadata.extension)? != path {
                    info!("Found duplicate orphaned file: {:?}", path);
                    let size = fs::metadata(&path).await?.len();
                    fs::remove_file(&path).await?;
                    return Ok((true, i64::try_from(size).unwrap_or(i64::MAX)));
                }

                // A crash can occur after the raw PUT is linked into place but
                // before its completed size reaches the metadata store. Remove
                // that uncommitted artifact and leave the ticket available for
                // a retry until its normal expiration.
                if metadata.is_video_ticket && metadata.size == 0 {
                    info!("Found uncommitted video upload: {:?}", path);
                    return remove_startup_artifact(&path).await;
                }

                if now >= metadata.expires_at {
                    info!(
                        "Found expired file: {:?} (expired at {})",
                        path, metadata.expires_at
                    );
                    let size = metadata.size;
                    fs::remove_file(&path).await.map_err(|e| {
                        AppError::InternalServerError(format!("Failed to remove expired file: {e}"))
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
                        AppError::InternalServerError(format!("Failed to remove file: {e}"))
                    })?;
                    self.store.remove(token).await?;
                    return Ok((true, size));
                }

                Ok((false, 0))
            }
            Err(AppError::NotFound(_)) => {
                info!("Found orphaned file (no metadata): {:?}", path);
                let metadata = fs::metadata(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to get file metadata: {e}"))
                })?;
                let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
                fs::remove_file(&path).await.map_err(|e| {
                    AppError::InternalServerError(format!("Failed to remove orphaned file: {e}"))
                })?;
                Ok((true, size))
            }
            Err(err) => {
                error!("Failed to load metadata for {}: {}", token, err);
                Err(AppError::InternalServerError(format!(
                    "Failed to load metadata: {err}"
                )))
            }
        }
    }

    fn get_storage_path(&self, token: &str) -> PathBuf {
        let subdir = token.get(..STORAGE_SUBDIR_DEPTH).unwrap_or(token);
        PathBuf::from(&self.config.upload_dir)
            .join(subdir)
            .join(token)
    }

    pub fn get_file_path(&self, token: &str, extension: &str) -> AppResult<PathBuf> {
        if TokenGenerator::parse_path(token) != Some(token) {
            return Err(AppError::InternalServerError(
                "refusing to construct a path for an invalid token".to_string(),
            ));
        }

        let mut path = self.get_storage_path(token);
        if extension.is_empty() {
            return Ok(path);
        }
        if !is_valid_extension(extension) {
            return Err(AppError::InternalServerError(
                "metadata contains an invalid file extension".to_string(),
            ));
        }

        path.set_extension(extension);
        Ok(path)
    }

    fn get_partial_path(&self, token: &str) -> AppResult<PathBuf> {
        if TokenGenerator::parse_path(token) != Some(token) {
            return Err(AppError::InternalServerError(
                "refusing to construct a partial path for an invalid token".to_string(),
            ));
        }

        let storage_path = self.get_storage_path(token);
        let parent = storage_path.parent().ok_or_else(|| {
            AppError::InternalServerError("upload path has no parent directory".to_string())
        })?;
        Ok(parent.join(format!(".{token}{PARTIAL_UPLOAD_SUFFIX}")))
    }

    pub async fn save_file(
        &self,
        token: &str,
        mut field: Field<'_>,
        filename: &str,
    ) -> AppResult<FileMetadata> {
        let extension = sanitized_extension(filename).unwrap_or_default();
        let full_path = self.get_file_path(token, &extension)?;
        let partial_path = self.get_partial_path(token)?;

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial_path)
            .await
            .map_err(|err| {
                AppError::InternalServerError(format!(
                    "failed to create partial upload {}: {err}",
                    partial_path.display()
                ))
            })?;
        let partial_file = PendingFile::new(partial_path.clone());
        let max_file_size = self.config.max_file_size;

        let write_result: AppResult<u64> = async move {
            let mut writer = BufWriter::with_capacity(FILE_WRITE_BUFFER_SIZE, file);
            let mut total_size = 0u64;

            while let Some(chunk) = field.chunk().await.map_err(AppError::from)? {
                total_size = total_size
                    .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                        AppError::BadRequest("file size exceeds supported range".into())
                    })?)
                    .ok_or_else(|| AppError::BadRequest("file size overflow".into()))?;

                if total_size > max_file_size {
                    return Err(AppError::PayloadTooLarge);
                }

                writer.write_all(&chunk).await?;
            }

            writer.flush().await?;
            Ok(total_size)
        }
        .await;

        let total_size = match write_result {
            Ok(total_size) => total_size,
            Err(err) => {
                partial_file.remove().await;
                return Err(err);
            }
        };
        let Ok(size) = i64::try_from(total_size) else {
            partial_file.remove().await;
            return Err(AppError::PayloadTooLarge);
        };
        let now = Utc::now();
        let Some(expires_at) = now.checked_add_signed(self.config.file_expiration) else {
            partial_file.remove().await;
            return Err(AppError::InternalServerError(
                "file expiration exceeds the supported calendar range".to_string(),
            ));
        };

        // Hard-linking publishes the completed upload while preserving the
        // create-new/no-overwrite guarantee if a token collision occurs.
        fs::hard_link(&partial_path, &full_path)
            .await
            .map_err(|err| {
                AppError::InternalServerError(format!(
                    "failed to commit upload {}: {err}",
                    full_path.display()
                ))
            })?;
        let committed_file = PendingFile::new(full_path.clone());

        fs::remove_file(&partial_path).await.map_err(|err| {
            AppError::InternalServerError(format!(
                "failed to remove partial upload {}: {err}",
                partial_path.display()
            ))
        })?;
        partial_file.persist();
        let is_image = is_image_extension(&extension);
        let metadata = FileMetadata {
            original_name: filename.to_string(),
            size,
            extension,
            is_video_ticket: false,
            uploaded_at: now,
            expires_at,
            download_count: 0,
            download_limit: if is_image {
                self.config.image_max_download_count
            } else {
                self.config.max_download_count
            },
        };

        if let Err(err) = self.store.save(token, metadata.clone()).await {
            if let Err(cleanup_err) = self.store.remove(token).await {
                error!(
                    "Failed to roll back metadata for {} after upload failure: {}",
                    token, cleanup_err
                );
            }
            committed_file.remove().await;
            return Err(err);
        }

        committed_file.persist();
        Ok(metadata)
    }

    /// Reserve a short-lived capability token for a video result. The token is
    /// stored before xAI receives the URL, but no file is created until the
    /// raw PUT arrives. `is_video_ticket` makes the capability explicit instead
    /// of treating every zero-byte MP4 as publicly writable.
    pub async fn create_video_ticket(&self) -> AppResult<String> {
        let now = Utc::now();
        let Some(expires_at) = now.checked_add_signed(self.config.file_expiration) else {
            return Err(AppError::InternalServerError(
                "file expiration exceeds the supported calendar range".to_string(),
            ));
        };

        for _ in 0..8 {
            let token = TokenGenerator::generate();
            let full_path = self.get_file_path(&token, VIDEO_EXTENSION)?;

            // A random token collision is extraordinarily unlikely, but do
            // not overwrite an existing file or metadata if one ever occurs.
            if fs::try_exists(&full_path).await? {
                continue;
            }
            match self.store.load(&token).await {
                Ok(_) => continue,
                Err(AppError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }

            let metadata = FileMetadata {
                original_name: VIDEO_FILENAME.to_string(),
                size: 0,
                extension: VIDEO_EXTENSION.to_string(),
                is_video_ticket: true,
                uploaded_at: now,
                expires_at,
                download_count: 0,
                download_limit: self.config.max_download_count,
            };
            self.store.save(&token, metadata).await?;
            return Ok(token);
        }

        Err(AppError::InternalServerError(
            "failed to allocate a unique video upload token".to_string(),
        ))
    }

    /// Stream one raw video PUT into the reserved path. Pending ticket metadata
    /// keeps the path unreadable until the final size is stored, while
    /// `create_new` serializes concurrent PUTs for the same token.
    pub async fn complete_video_upload(&self, token: &str, body: Body) -> AppResult<i64> {
        if TokenGenerator::parse_path(token) != Some(token) {
            return Err(AppError::BadRequest(
                "Invalid video upload token".to_string(),
            ));
        }

        let metadata = self.store.load(token).await?;
        if !metadata.is_video_ticket || metadata.extension != VIDEO_EXTENSION {
            return Err(AppError::BadRequest(
                "Token is not a video upload ticket".to_string(),
            ));
        }
        if metadata.size != 0 {
            // PutObject targets are retryable. If xAI lost the first success
            // response, acknowledge the retry without replacing stored bytes.
            return Ok(metadata.size);
        }
        if Utc::now() >= metadata.expires_at {
            return Err(AppError::Gone("Video upload ticket expired".to_string()));
        }

        let full_path = self.get_file_path(token, VIDEO_EXTENSION)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full_path)
            .await
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(AppError::Conflict(
                    "Video upload ticket is already in progress or has been used".to_string(),
                ));
            }
            Err(err) => {
                return Err(AppError::InternalServerError(format!(
                    "failed to create video upload {}: {err}",
                    full_path.display()
                )));
            }
        };
        let pending_file = PendingFile::new(full_path);
        let max_file_size = self.config.max_file_size;

        let write_result: AppResult<u64> = async move {
            let mut writer = BufWriter::with_capacity(FILE_WRITE_BUFFER_SIZE, file);
            let mut stream = body.into_data_stream();
            let mut total_size = 0u64;

            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    AppError::BadRequest("failed to read video upload body".to_string())
                })?;
                total_size = total_size
                    .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                        AppError::BadRequest("file size exceeds supported range".to_string())
                    })?)
                    .ok_or_else(|| AppError::BadRequest("file size overflow".to_string()))?;

                if total_size > max_file_size {
                    return Err(AppError::PayloadTooLarge);
                }

                writer.write_all(&chunk).await?;
            }

            writer.flush().await?;
            Ok(total_size)
        }
        .await;

        let total_size = match write_result {
            Ok(total_size) => total_size,
            Err(err) => {
                pending_file.remove().await;
                return Err(err);
            }
        };
        if total_size == 0 {
            pending_file.remove().await;
            return Err(AppError::BadRequest(
                "Video upload body cannot be empty".to_string(),
            ));
        }
        if Utc::now() >= metadata.expires_at {
            pending_file.remove().await;
            return Err(AppError::Gone("Video upload ticket expired".to_string()));
        }

        let Ok(size) = i64::try_from(total_size) else {
            pending_file.remove().await;
            return Err(AppError::PayloadTooLarge);
        };

        let mut completed = metadata;
        completed.size = size;
        if let Err(err) = self.store.save(token, completed).await {
            pending_file.remove().await;
            if let Err(cleanup_err) = self.store.remove(token).await {
                error!(
                    "Failed to roll back metadata for {} after video upload failure: {}",
                    token, cleanup_err
                );
            }
            return Err(err);
        }

        pending_file.persist();
        Ok(size)
    }

    pub async fn remove_file(&self, token: &str, metadata: &FileMetadata) -> AppResult<()> {
        let path = self.get_file_path(token, &metadata.extension)?;
        if let Err(err) = fs::remove_file(&path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(AppError::InternalServerError(format!(
                "failed to remove file {}: {err}",
                path.display()
            )));
        }
        self.store.remove(token).await?;

        info!("Removed: {} -> {}", metadata.original_name, token);
        Ok(())
    }

    pub async fn cleanup_worker(server: Arc<Self>) {
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
                Ok(expired_files) => {
                    if expired_files.is_empty() {
                        break;
                    }

                    let count = expired_files.len();

                    let mut join_set = JoinSet::new();
                    for (token, metadata) in expired_files {
                        let server = self.clone();
                        join_set.spawn(async move { server.remove_file(&token, &metadata).await });
                    }

                    let mut removed_count = 0usize;
                    while let Some(result) = join_set.join_next().await {
                        match result {
                            Ok(Ok(())) => removed_count += 1,
                            Ok(Err(err)) => error!("Cleanup failed: {}", err),
                            Err(err) => error!("Cleanup task panicked: {}", err),
                        }
                    }

                    info!("Cleaned up {} expired files", removed_count);

                    if count < CLEANUP_BATCH_SIZE || removed_count < count {
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

fn auxiliary_token<'a>(filename: &'a str, suffix: &str) -> Option<&'a str> {
    let token = filename.strip_prefix('.')?.strip_suffix(suffix)?;
    (TokenGenerator::parse_path(token) == Some(token)).then_some(token)
}

async fn remove_startup_artifact(path: &Path) -> AppResult<(bool, i64)> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((false, 0)),
        Err(err) => return Err(AppError::from(err)),
    };

    let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    if let Err(err) = fs::remove_file(path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(AppError::InternalServerError(format!(
            "failed to remove stale upload artifact {}: {err}",
            path.display()
        )));
    }
    info!("Removed incomplete upload artifact: {:?}", path);
    Ok((true, size))
}

fn record_cleanup_result(
    result: Result<AppResult<(bool, i64)>, tokio::task::JoinError>,
    cleaned_count: &mut usize,
    total_size: &mut i64,
    error_count: &mut usize,
) {
    match result {
        Ok(Ok((true, size))) => {
            *cleaned_count += 1;
            *total_size = total_size.saturating_add(size);
        }
        Ok(Ok((false, _))) => {}
        Ok(Err(err)) => {
            *error_count += 1;
            error!("Error cleaning file during startup cleanup: {}", err);
        }
        Err(err) => {
            *error_count += 1;
            error!("Startup cleanup task panicked: {}", err);
        }
    }
}

fn sanitized_extension(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| is_valid_extension(extension) && !extension.is_empty())
}

fn is_image_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif" | "svg" | "ico" | "bmp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn extension_is_normalized_and_unsafe_values_are_ignored() {
        assert_eq!(sanitized_extension("photo.JPEG").as_deref(), Some("jpeg"));
        assert_eq!(sanitized_extension("archive.tar.gz").as_deref(), Some("gz"));
        assert_eq!(sanitized_extension("file.bad-extension"), None);
        assert_eq!(sanitized_extension("file.abcdefghijklmnopq"), None);
        assert_eq!(sanitized_extension("no-extension"), None);
    }

    #[tokio::test]
    async fn startup_cleanup_removes_incomplete_and_orphaned_files() {
        let upload_dir = tempfile::tempdir().unwrap();
        let server = FileServer::new(ServerConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            upload_dir: upload_dir.path().to_string_lossy().into_owned(),
            max_file_size: 1024,
            file_expiration: Duration::hours(1),
            cleanup_interval: Duration::minutes(1),
            max_download_count: 0,
            image_max_download_count: 0,
            public_base_url: None,
            api_key: None,
            redis_addr: None,
            redis_db: 0,
        })
        .await
        .unwrap();
        server.initialize_storage().await.unwrap();

        let partial_token = TokenGenerator::generate();
        let partial_path = server.get_partial_path(&partial_token).unwrap();
        fs::create_dir_all(partial_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&partial_path, b"partial").await.unwrap();

        let orphan_token = TokenGenerator::generate();
        let orphan_path = server.get_file_path(&orphan_token, "txt").unwrap();
        fs::create_dir_all(orphan_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&orphan_path, b"orphan").await.unwrap();

        let unknown_path = upload_dir.path().join("xx").join("unrecognized.file");
        fs::create_dir_all(unknown_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&unknown_path, b"unknown").await.unwrap();

        let valid_token = TokenGenerator::generate();
        let valid_path = server.get_file_path(&valid_token, "txt").unwrap();
        fs::create_dir_all(valid_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&valid_path, b"valid").await.unwrap();
        let now = Utc::now();
        server
            .store
            .save(
                &valid_token,
                FileMetadata {
                    original_name: "valid.txt".to_string(),
                    size: 5,
                    extension: "txt".to_string(),
                    is_video_ticket: false,
                    uploaded_at: now,
                    expires_at: now + Duration::hours(1),
                    download_count: 0,
                    download_limit: 0,
                },
            )
            .await
            .unwrap();

        let pending_video_token = server.create_video_ticket().await.unwrap();
        let pending_video_path = server
            .get_file_path(&pending_video_token, VIDEO_EXTENSION)
            .unwrap();
        fs::create_dir_all(pending_video_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&pending_video_path, b"uncommitted")
            .await
            .unwrap();

        server.startup_cleanup().await.unwrap();
        assert!(!partial_path.exists());
        assert!(!orphan_path.exists());
        assert!(!unknown_path.exists());
        assert!(valid_path.exists());
        assert!(!pending_video_path.exists());
        assert!(
            server
                .store
                .load(&pending_video_token)
                .await
                .unwrap()
                .is_video_ticket
        );
    }
}
