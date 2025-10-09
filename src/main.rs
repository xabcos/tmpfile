use axum::{
    Json, Router,
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use rand::{Rng, thread_rng};
use redis::{AsyncCommands, Client, aio::ConnectionManager};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration as StdDuration,
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    time::interval,
};
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, error, info, warn};

// ===========================
// Configuration Constants
// ===========================

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_UPLOAD_DIR: &str = "./uploads";
const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024; // 256MB
const DEFAULT_FILE_EXPIRATION: i64 = 36 * 3600; // 36 hours in seconds
const DEFAULT_MAX_DOWNLOAD_COUNT: i32 = 0; // 0 means unlimited

const MEMORY_SHARD_COUNT: usize = 16;
const CLEANUP_BATCH_SIZE: usize = 128;
const STORAGE_SUBDIR_DEPTH: usize = 2;

const TOKEN_CHARSET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
const TOKEN_LENGTH: usize = 8;
const TOKEN_TIME_FORMAT: &str = "%m%d%H%M";

// ===========================
// Core Data Structures
// ===========================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileMetadata {
    original_name: String,
    storage_path: String,
    size: i64,
    extension: String,
    uploaded_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    #[serde(skip)]
    download_count: Arc<AtomicI32>,
    download_limit: i32,
}

impl FileMetadata {
    fn get_download_count(&self) -> i32 {
        self.download_count.load(Ordering::Relaxed)
    }

    fn increment_download_count(&self) -> i32 {
        self.download_count.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[derive(Debug, Serialize)]
struct UploadResponse {
    direct_link: String,
    filename: String,
    size: i64,
    token: String,
    expires_at: DateTime<Utc>,
    expires_in: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_downloads: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_downloads: Option<i32>,
}

// ===========================
// Storage Trait
// ===========================

#[async_trait::async_trait]
trait MetadataStore: Send + Sync {
    async fn save(
        &self,
        token: &str,
        metadata: FileMetadata,
        ttl: StdDuration,
    ) -> Result<(), String>;
    async fn load(&self, token: &str) -> Result<FileMetadata, String>;
    async fn increment_downloads(&self, token: &str) -> Result<i32, String>;
    async fn remove(&self, token: &str) -> Result<(), String>;
    async fn list_expired(&self, limit: usize) -> Result<(Vec<String>, Vec<FileMetadata>), String>;
}

// ===========================
// Memory Storage Implementation
// ===========================

type MemoryShard = Arc<RwLock<HashMap<String, (FileMetadata, DateTime<Utc>)>>>;

struct MemoryStore {
    shards: Vec<MemoryShard>,
}

impl MemoryStore {
    fn new() -> Self {
        let shards = (0..MEMORY_SHARD_COUNT)
            .map(|_| Arc::new(RwLock::new(HashMap::new())))
            .collect();
        Self { shards }
    }

    fn get_shard_for_token(&self, token: &str) -> &MemoryShard {
        let hash = token
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        &self.shards[(hash as usize) % MEMORY_SHARD_COUNT]
    }
}

#[async_trait::async_trait]
impl MetadataStore for MemoryStore {
    async fn save(
        &self,
        token: &str,
        metadata: FileMetadata,
        ttl: StdDuration,
    ) -> Result<(), String> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        let expires_at = Utc::now() + Duration::from_std(ttl).unwrap();
        guard.insert(token.to_string(), (metadata, expires_at));
        Ok(())
    }

    async fn load(&self, token: &str) -> Result<FileMetadata, String> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read();
        guard
            .get(token)
            .map(|(metadata, _)| metadata.clone())
            .ok_or_else(|| "file not found".to_string())
    }

    async fn increment_downloads(&self, token: &str) -> Result<i32, String> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read();
        guard
            .get(token)
            .map(|(metadata, _)| metadata.increment_download_count())
            .ok_or_else(|| "file not found".to_string())
    }

    async fn remove(&self, token: &str) -> Result<(), String> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        guard.remove(token);
        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> Result<(Vec<String>, Vec<FileMetadata>), String> {
        let mut tokens = Vec::new();
        let mut metadata = Vec::new();
        let now = Utc::now();

        for shard in &self.shards {
            let guard = shard.read();
            for (token, (meta, expires_at)) in guard.iter() {
                if now > *expires_at || now > meta.expires_at {
                    tokens.push(token.clone());
                    metadata.push(meta.clone());
                    if tokens.len() >= limit {
                        return Ok((tokens, metadata));
                    }
                }
            }
        }

        Ok((tokens, metadata))
    }
}

// ===========================
// Redis Storage Implementation
// ===========================

struct RedisStore {
    conn_manager: ConnectionManager,
}

impl RedisStore {
    async fn new(addr: &str, db: i64) -> Result<Self, String> {
        let client = Client::open(format!("redis://{}/{}", addr, db))
            .map_err(|e| format!("Failed to create Redis client: {}", e))?;

        let conn_manager = ConnectionManager::new(client)
            .await
            .map_err(|e| format!("Redis connection failed: {}", e))?;

        // Test connection
        let mut conn = conn_manager.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| format!("Redis ping failed: {}", e))?;

        info!("Redis connection established");
        Ok(Self { conn_manager })
    }

    fn metadata_key(&self, token: &str) -> String {
        format!("file:meta:{}", token)
    }

    const EXPIRY_SET_KEY: &'static str = "file:expiry:set";
}

#[async_trait::async_trait]
impl MetadataStore for RedisStore {
    async fn save(
        &self,
        token: &str,
        metadata: FileMetadata,
        ttl: StdDuration,
    ) -> Result<(), String> {
        let mut conn = self.conn_manager.clone();

        let serializable = SerializableMetadata::from(&metadata);
        let data = serde_json::to_string(&serializable)
            .map_err(|e| format!("Failed to serialize metadata: {}", e))?;

        let key = self.metadata_key(token);
        let ttl_secs = ttl.as_secs() as u64;

        redis::pipe()
            .atomic()
            .set_ex(&key, data, ttl_secs)
            .zadd(Self::EXPIRY_SET_KEY, token, metadata.expires_at.timestamp())
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| format!("Failed to save to Redis: {}", e))?;

        Ok(())
    }

    async fn load(&self, token: &str) -> Result<FileMetadata, String> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let data: String = conn.get(&key).await.map_err(|e| {
            if e.kind() == redis::ErrorKind::TypeError {
                "file not found".to_string()
            } else {
                format!("Failed to get metadata: {}", e)
            }
        })?;

        let serializable: SerializableMetadata = serde_json::from_str(&data)
            .map_err(|e| format!("Failed to deserialize metadata: {}", e))?;

        Ok(serializable.into())
    }

    async fn increment_downloads(&self, token: &str) -> Result<i32, String> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let data: String = conn
            .get(&key)
            .await
            .map_err(|_| "file not found".to_string())?;

        let mut metadata: SerializableMetadata = serde_json::from_str(&data)
            .map_err(|e| format!("Failed to deserialize metadata: {}", e))?;

        metadata.download_count += 1;
        let new_count = metadata.download_count;

        let ttl: i64 = conn
            .ttl(&key)
            .await
            .map_err(|e| format!("Failed to get TTL: {}", e))?;

        if ttl > 0 {
            let new_data = serde_json::to_string(&metadata)
                .map_err(|e| format!("Failed to serialize metadata: {}", e))?;
            conn.set_ex::<_, _, ()>(&key, new_data, ttl as u64)
                .await
                .map_err(|e| format!("Failed to update metadata: {}", e))?;
        }

        Ok(new_count)
    }

    async fn remove(&self, token: &str) -> Result<(), String> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        redis::pipe()
            .atomic()
            .del(&key)
            .zrem(Self::EXPIRY_SET_KEY, token)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| format!("Failed to remove from Redis: {}", e))?;

        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> Result<(Vec<String>, Vec<FileMetadata>), String> {
        let mut conn = self.conn_manager.clone();
        let now = Utc::now().timestamp();

        let tokens: Vec<String> = conn
            .zrangebyscore_limit(Self::EXPIRY_SET_KEY, 0, now, 0, limit as isize)
            .await
            .map_err(|e| format!("Failed to get expired tokens: {}", e))?;

        if tokens.is_empty() {
            return Ok((vec![], vec![]));
        }

        let mut valid_tokens = Vec::new();
        let mut metadata_list = Vec::new();

        for token in tokens {
            let key = self.metadata_key(&token);
            if let Ok(data) = conn.get::<_, String>(&key).await {
                if let Ok(serializable) = serde_json::from_str::<SerializableMetadata>(&data) {
                    valid_tokens.push(token);
                    metadata_list.push(serializable.into());
                }
            }
        }

        Ok((valid_tokens, metadata_list))
    }
}

// Helper struct for Redis serialization
#[derive(Serialize, Deserialize)]
struct SerializableMetadata {
    original_name: String,
    storage_path: String,
    size: i64,
    extension: String,
    uploaded_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    download_count: i32,
    download_limit: i32,
}

impl From<&FileMetadata> for SerializableMetadata {
    fn from(meta: &FileMetadata) -> Self {
        Self {
            original_name: meta.original_name.clone(),
            storage_path: meta.storage_path.clone(),
            size: meta.size,
            extension: meta.extension.clone(),
            uploaded_at: meta.uploaded_at,
            expires_at: meta.expires_at,
            download_count: meta.get_download_count(),
            download_limit: meta.download_limit,
        }
    }
}

impl From<SerializableMetadata> for FileMetadata {
    fn from(s: SerializableMetadata) -> Self {
        Self {
            original_name: s.original_name,
            storage_path: s.storage_path,
            size: s.size,
            extension: s.extension,
            uploaded_at: s.uploaded_at,
            expires_at: s.expires_at,
            download_count: Arc::new(AtomicI32::new(s.download_count)),
            download_limit: s.download_limit,
        }
    }
}

// ===========================
// Token Generation
// ===========================

struct TokenGenerator;

impl TokenGenerator {
    fn generate() -> String {
        let mut rng = thread_rng();
        let charset: Vec<char> = TOKEN_CHARSET.chars().collect();

        let random_part: String = (0..TOKEN_LENGTH)
            .map(|_| charset[rng.gen_range(0..charset.len())])
            .collect();

        let time_part = Utc::now().format(TOKEN_TIME_FORMAT).to_string();
        format!("{}{}", random_part, time_part)
    }
}

// ===========================
// Server Configuration
// ===========================

#[derive(Clone)]
struct ServerConfig {
    listen_addr: String,
    upload_dir: String,
    max_file_size: u64,
    file_expiration: Duration,
    cleanup_interval: Duration,
    max_download_count: i32,
    api_key: Option<String>,
    redis_addr: Option<String>,
    redis_db: i64,
}

// ===========================
// File Server
// ===========================

#[derive(Clone)]
struct FileServer {
    config: Arc<ServerConfig>,
    store: Arc<dyn MetadataStore>,
}

impl FileServer {
    async fn new(config: ServerConfig) -> Result<Self, String> {
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

    async fn initialize_storage(&self) -> Result<(), String> {
        fs::create_dir_all(&self.config.upload_dir)
            .await
            .map_err(|e| format!("Failed to create upload directory: {}", e))?;

        // Subdirectories will be created on-demand when saving files
        debug!(
            "Upload directory initialized at: {}",
            self.config.upload_dir
        );
        Ok(())
    }

    async fn startup_cleanup(&self) -> Result<(), String> {
        info!("Starting cleanup of orphaned files...");
        let start_time = std::time::Instant::now();
        let mut cleaned_count = 0;
        let mut total_size = 0i64;
        let mut error_count = 0;

        let mut entries = fs::read_dir(&self.config.upload_dir)
            .await
            .map_err(|e| format!("Failed to read upload directory: {}", e))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("Failed to read directory entry: {}", e))?
        {
            let path = entry.path();
            if path.is_dir() {
                let mut subdir_entries = fs::read_dir(&path)
                    .await
                    .map_err(|e| format!("Failed to read subdirectory: {}", e))?;

                while let Some(file_entry) = subdir_entries
                    .next_entry()
                    .await
                    .map_err(|e| format!("Failed to read file entry: {}", e))?
                {
                    let file_path = file_entry.path();
                    if file_path.is_file() {
                        match self.check_and_clean_file(&file_path).await {
                            Ok((cleaned, size)) => {
                                if cleaned {
                                    cleaned_count += 1;
                                    total_size += size;
                                }
                            }
                            Err(e) => {
                                error_count += 1;
                                error!("Error cleaning file {:?}: {}", file_path, e);
                            }
                        }
                    }
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

    async fn check_and_clean_file(&self, path: &PathBuf) -> Result<(bool, i64), String> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "Invalid filename".to_string())?;

        let token = if filename.len() >= 16 {
            &filename[..16]
        } else {
            debug!("Found unrecognized file: {:?}", path);
            return Ok((false, 0));
        };

        match self.store.load(token).await {
            Ok(metadata) => {
                let now = Utc::now();

                if now > metadata.expires_at {
                    debug!(
                        "Found expired file: {:?} (expired at {})",
                        path, metadata.expires_at
                    );
                    let size = metadata.size;
                    fs::remove_file(path)
                        .await
                        .map_err(|e| format!("Failed to remove expired file: {}", e))?;
                    self.store.remove(token).await?;
                    return Ok((true, size));
                }

                if metadata.download_limit > 0
                    && metadata.get_download_count() >= metadata.download_limit
                {
                    debug!(
                        "Found file exceeding download limit: {:?} ({}/{})",
                        path,
                        metadata.get_download_count(),
                        metadata.download_limit
                    );
                    let size = metadata.size;
                    fs::remove_file(path)
                        .await
                        .map_err(|e| format!("Failed to remove file: {}", e))?;
                    self.store.remove(token).await?;
                    return Ok((true, size));
                }

                Ok((false, 0))
            }
            Err(_) => {
                debug!("Found orphaned file (no metadata): {:?}", path);
                let metadata = fs::metadata(path)
                    .await
                    .map_err(|e| format!("Failed to get file metadata: {}", e))?;
                let size = metadata.len() as i64;
                fs::remove_file(path)
                    .await
                    .map_err(|e| format!("Failed to remove orphaned file: {}", e))?;
                Ok((true, size))
            }
        }
    }

    fn get_storage_path(&self, token: &str) -> PathBuf {
        let subdir = &token[..STORAGE_SUBDIR_DEPTH];
        PathBuf::from(&self.config.upload_dir)
            .join(subdir)
            .join(token)
    }

    async fn save_file(
        &self,
        token: &str,
        data: Bytes,
        filename: &str,
    ) -> Result<FileMetadata, String> {
        let extension = PathBuf::from(filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default();

        let storage_path = self.get_storage_path(token);
        let full_path = PathBuf::from(format!("{}{}", storage_path.display(), extension));

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        let mut file = File::create(&full_path)
            .await
            .map_err(|e| format!("Failed to create file: {}", e))?;

        let size = data.len() as i64;
        file.write_all(&data)
            .await
            .map_err(|e| format!("Failed to write file: {}", e))?;

        let now = Utc::now();
        Ok(FileMetadata {
            original_name: filename.to_string(),
            storage_path: full_path.to_string_lossy().to_string(),
            size,
            extension,
            uploaded_at: now,
            expires_at: now + self.config.file_expiration,
            download_count: Arc::new(AtomicI32::new(0)),
            download_limit: self.config.max_download_count,
        })
    }

    async fn remove_file(&self, token: &str, metadata: &FileMetadata) {
        if let Err(e) = fs::remove_file(&metadata.storage_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                error!("Failed to remove file {}: {}", metadata.storage_path, e);
            }
        }

        if let Err(e) = self.store.remove(token).await {
            error!("Failed to remove metadata for token {}: {}", token, e);
        }

        info!("Removed: {} -> {}", metadata.original_name, token);
    }

    async fn cleanup_worker(server: Arc<FileServer>) {
        let mut interval = interval(server.config.cleanup_interval.to_std().unwrap());

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

                    let tasks: Vec<_> = tokens
                        .into_iter()
                        .zip(metadata_list.into_iter())
                        .map(|(token, metadata)| {
                            let server = self.clone();
                            tokio::spawn(async move {
                                server.remove_file(&token, &metadata).await;
                            })
                        })
                        .collect();

                    for task in tasks {
                        let _ = task.await;
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

// ===========================
// HTTP Handlers
// ===========================

async fn handle_upload(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, StatusCode> {
    if let Some(api_key) = &server.config.api_key {
        let auth_header = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        let provided_key = auth_header.strip_prefix("Bearer ").unwrap_or("");
        if provided_key != api_key {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        if field.name() == Some("file") {
            let filename = field
                .file_name()
                .ok_or(StatusCode::BAD_REQUEST)?
                .to_string();

            if filename.is_empty() {
                continue;
            }

            let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;

            if data.len() > server.config.max_file_size as usize {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }

            let token = TokenGenerator::generate();
            let metadata = server
                .save_file(&token, data, &filename)
                .await
                .map_err(|e| {
                    error!("Failed to save file: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            let ttl = if server.config.redis_addr.is_some() {
                server.config.file_expiration + server.config.cleanup_interval
            } else {
                server.config.file_expiration
            };

            server
                .store
                .save(&token, metadata.clone(), ttl.to_std().unwrap())
                .await
                .map_err(|e| {
                    error!("Failed to save metadata: {}", e);
                    let _ = fs::remove_file(&metadata.storage_path);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            let scheme = if headers
                .get("x-forwarded-proto")
                .map(|v| v == "https")
                .unwrap_or(false)
            {
                "https"
            } else {
                "http"
            };

            let host = headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("localhost");

            let mut response = UploadResponse {
                direct_link: format!(
                    "{}://{}/files/{}{}",
                    scheme, host, token, metadata.extension
                ),
                filename: metadata.original_name.clone(),
                size: metadata.size,
                token: token.clone(),
                expires_at: metadata.expires_at,
                expires_in: format!("{}h", server.config.file_expiration.num_hours()),
                max_downloads: None,
                remaining_downloads: None,
            };

            if server.config.max_download_count > 0 {
                response.max_downloads = Some(server.config.max_download_count);
                response.remaining_downloads = Some(server.config.max_download_count);
            }

            info!(
                "Uploaded: {} ({:.2}MB) -> {}",
                metadata.original_name,
                metadata.size as f64 / (1024.0 * 1024.0),
                token
            );

            return Ok(Json(response));
        }
    }

    Err(StatusCode::BAD_REQUEST)
}

async fn handle_download(
    State(server): State<Arc<FileServer>>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let token = if path.len() >= 16 {
        &path[..16]
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let metadata = server
        .store
        .load(token)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    if Utc::now() > metadata.expires_at {
        return Err(StatusCode::GONE);
    }

    if metadata.download_limit > 0 && metadata.get_download_count() >= metadata.download_limit {
        let server_clone = server.clone();
        let token_owned = token.to_string();
        let metadata_clone = metadata.clone();
        tokio::spawn(async move {
            server_clone
                .remove_file(&token_owned, &metadata_clone)
                .await;
        });
        return Err(StatusCode::GONE);
    }

    let file_data = fs::read(&metadata.storage_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let new_count = server
        .store
        .increment_downloads(token)
        .await
        .unwrap_or(metadata.get_download_count() + 1);

    info!(
        "Download: {} -> {} (count: {}/{})",
        metadata.original_name,
        token,
        new_count,
        if metadata.download_limit > 0 {
            metadata.download_limit.to_string()
        } else {
            "unlimited".to_string()
        }
    );

    if metadata.download_limit > 0 && new_count >= metadata.download_limit {
        let server_clone = server.clone();
        let token_owned = token.to_string();
        let metadata_clone = metadata.clone();
        tokio::spawn(async move {
            server_clone
                .remove_file(&token_owned, &metadata_clone)
                .await;
        });
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            metadata.original_name
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.size.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    Ok((headers, file_data))
}

// ===========================
// Configuration Helpers
// ===========================

fn parse_env_duration(key: &str, default: i64) -> Duration {
    env::var(key)
        .ok()
        .and_then(|v| {
            v.parse::<i64>().ok().map(Duration::seconds).or_else(|| {
                if v.ends_with('h') {
                    v[..v.len() - 1].parse::<i64>().ok().map(Duration::hours)
                } else if v.ends_with('d') {
                    v[..v.len() - 1].parse::<i64>().ok().map(Duration::days)
                } else if v.ends_with('m') {
                    v[..v.len() - 1].parse::<i64>().ok().map(Duration::minutes)
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| {
            info!("Using default {} seconds for {}", default, key);
            Duration::seconds(default)
        })
}

fn parse_env_int(key: &str, default: i32) -> i32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            info!("Using default {} for {}", default, key);
            default
        })
}

fn parse_env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            info!("Using default {} for {}", default, key);
            default
        })
}

// ===========================
// Main Entry Point
// ===========================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig {
        listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string()),
        upload_dir: env::var("UPLOAD_DIR").unwrap_or_else(|_| DEFAULT_UPLOAD_DIR.to_string()),
        max_file_size: parse_env_u64("MAX_FILE_SIZE", DEFAULT_MAX_FILE_SIZE),
        file_expiration: parse_env_duration("FILE_EXPIRATION", DEFAULT_FILE_EXPIRATION),
        cleanup_interval: parse_env_duration("FILE_EXPIRATION", DEFAULT_FILE_EXPIRATION) / 2,
        max_download_count: parse_env_int("MAX_DOWNLOAD_COUNT", DEFAULT_MAX_DOWNLOAD_COUNT),
        api_key: env::var("API_KEY").ok().filter(|s| !s.is_empty()),
        redis_addr: env::var("REDIS_ADDR").ok().filter(|s| !s.is_empty()),
        redis_db: env::var("REDIS_DB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };

    info!("File server starting on {}", config.listen_addr);
    info!(
        "Configuration: MaxFileSize={}MB, FileExpiration={}h, MaxDownloads={}",
        config.max_file_size / (1024 * 1024),
        config.file_expiration.num_hours(),
        if config.max_download_count > 0 {
            config.max_download_count.to_string()
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

    let app = Router::new()
        .route("/upload", post(handle_upload))
        .route("/files/{path}", get(handle_download))
        .layer(
            ServiceBuilder::new().layer(RequestBodyLimitLayer::new(config.max_file_size as usize)),
        )
        .with_state(server);

    let addr: std::net::SocketAddr = config.listen_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
