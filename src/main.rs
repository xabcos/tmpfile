use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path, State, multipart::Field},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use bytes::BytesMut;
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use rand::{Rng, distributions::Uniform, thread_rng};
use redis::{AsyncCommands, Client, RedisError, Script, aio::ConnectionManager};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fmt,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration as StdDuration,
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    sync::Semaphore,
    task::JoinSet,
    time::interval,
};
use tokio_util::io::ReaderStream;
use tower::ServiceBuilder;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{error, info, warn};

// ===========================
// Configuration Constants
// ===========================

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_UPLOAD_DIR: &str = "./uploads";
const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024; // 256MB
const DEFAULT_FILE_EXPIRATION: i64 = 36 * 3600; // 36 hours in seconds
const DEFAULT_MAX_DOWNLOAD_COUNT: i32 = 0; // 0 means unlimited

const MEMORY_SHARD_COUNT: usize = 64;
const CLEANUP_BATCH_SIZE: usize = 128;
const STORAGE_SUBDIR_DEPTH: usize = 2;
const STARTUP_CLEANUP_CONCURRENCY: usize = 32;
const MIN_CLEANUP_INTERVAL_SECS: i64 = 60;
const MAX_CLEANUP_INTERVAL_SECS: i64 = 24 * 3600;
const UPLOAD_BUFFER_SIZE: usize = 128 * 1024;
const UPLOAD_BUFFER_POOL_SIZE: usize = 32;
const UPLOAD_BUFFER_MAX_RETENTION: usize = 256 * 1024;

static UPLOAD_BUFFER_POOL: LazyLock<Mutex<Vec<BytesMut>>> =
    LazyLock::new(|| Mutex::new(Vec::with_capacity(UPLOAD_BUFFER_POOL_SIZE)));

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
    download_count: i32,
    download_limit: i32,
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

#[derive(Debug, Serialize)]
struct FileItem {
    token: String,
    filename: String,
    size: i64,
    uploaded_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    download_count: i32,
    download_link: String,
}

#[derive(Debug, Serialize)]
struct ListFilesResponse {
    files: Vec<FileItem>,
    total: usize,
    page: usize,
    size: usize,
    total_pages: usize,
}

#[derive(Debug)]
enum MetadataError {
    NotFound,
    Backend(String),
    Serialization(String),
    Deserialization(String),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataError::NotFound => write!(f, "file not found"),
            MetadataError::Backend(err) => write!(f, "{err}"),
            MetadataError::Serialization(err) => write!(f, "serialization error: {err}"),
            MetadataError::Deserialization(err) => write!(f, "deserialization error: {err}"),
        }
    }
}

impl std::error::Error for MetadataError {}

impl From<RedisError> for MetadataError {
    fn from(error: RedisError) -> Self {
        MetadataError::Backend(error.to_string())
    }
}

impl From<serde_json::Error> for MetadataError {
    fn from(error: serde_json::Error) -> Self {
        MetadataError::Deserialization(error.to_string())
    }
}

type MetadataResult<T> = Result<T, MetadataError>;

#[async_trait::async_trait]
trait MetadataStore: Send + Sync {
    async fn save(
        &self,
        token: &str,
        metadata: FileMetadata,
        ttl: StdDuration,
    ) -> MetadataResult<()>;
    async fn load(&self, token: &str) -> MetadataResult<FileMetadata>;
    async fn increment_downloads(&self, token: &str) -> MetadataResult<i32>;
    async fn remove(&self, token: &str) -> MetadataResult<()>;
    async fn list_expired(&self, limit: usize) -> MetadataResult<(Vec<String>, Vec<FileMetadata>)>;
    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: &str,
        desc: bool,
    ) -> MetadataResult<(Vec<(String, FileMetadata)>, usize)>;
}

fn sort_and_paginate_files(
    files: &mut Vec<(String, FileMetadata)>,
    offset: usize,
    limit: usize,
    sort_by: &str,
    desc: bool,
) -> (Vec<(String, FileMetadata)>, usize) {
    match sort_by {
        "size" => files.sort_by(|a, b| {
            if desc {
                b.1.size.cmp(&a.1.size)
            } else {
                a.1.size.cmp(&b.1.size)
            }
        }),
        "name" => files.sort_by(|a, b| {
            if desc {
                b.1.original_name.cmp(&a.1.original_name)
            } else {
                a.1.original_name.cmp(&b.1.original_name)
            }
        }),
        _ => files.sort_by(|a, b| {
            if desc {
                b.1.uploaded_at.cmp(&a.1.uploaded_at)
            } else {
                a.1.uploaded_at.cmp(&b.1.uploaded_at)
            }
        }),
    }

    let total = files.len();
    let start = offset.min(total);
    let end = (offset + limit).min(total);
    (files[start..end].to_vec(), total)
}

// ===========================
// Memory Storage Implementation
// ===========================

type MemoryShard = Arc<RwLock<HashMap<String, FileMetadata>>>;

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
        mut metadata: FileMetadata,
        ttl: StdDuration,
    ) -> MetadataResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        let ttl_duration =
            Duration::from_std(ttl).map_err(|e| MetadataError::Backend(e.to_string()))?;
        metadata.expires_at = Utc::now() + ttl_duration;
        guard.insert(token.to_string(), metadata);
        Ok(())
    }

    async fn load(&self, token: &str) -> MetadataResult<FileMetadata> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read();
        guard
            .get(token)
            .map(|metadata| metadata.clone())
            .ok_or(MetadataError::NotFound)
    }

    async fn increment_downloads(&self, token: &str) -> MetadataResult<i32> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        let metadata = guard.get_mut(token).ok_or(MetadataError::NotFound)?;
        metadata.download_count += 1;
        Ok(metadata.download_count)
    }

    async fn remove(&self, token: &str) -> MetadataResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        guard.remove(token);
        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> MetadataResult<(Vec<String>, Vec<FileMetadata>)> {
        let mut tokens = Vec::new();
        let mut metadata = Vec::new();
        let now = Utc::now();

        for shard in &self.shards {
            let guard = shard.read();
            for (token, meta) in guard.iter() {
                if now > meta.expires_at {
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

    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: &str,
        desc: bool,
    ) -> MetadataResult<(Vec<(String, FileMetadata)>, usize)> {
        let mut all_files = Vec::new();
        let now = Utc::now();

        // Collect all non-expired files
        for shard in &self.shards {
            let guard = shard.read();
            for (token, meta) in guard.iter() {
                if now < meta.expires_at {
                    all_files.push((token.clone(), meta.clone()));
                }
            }
        }

        let (paginated, total) =
            sort_and_paginate_files(&mut all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
    }
}

// ===========================
// Redis Storage Implementation
// ===========================

struct RedisStore {
    conn_manager: ConnectionManager,
    increment_script: Script,
}

impl RedisStore {
    async fn new(addr: &str, db: i64) -> MetadataResult<Self> {
        let client = Client::open(format!("redis://{}/{}", addr, db))
            .map_err(|e| MetadataError::Backend(format!("Failed to create Redis client: {e}")))?;

        let conn_manager = ConnectionManager::new(client)
            .await
            .map_err(|e| MetadataError::Backend(format!("Redis connection failed: {e}")))?;

        // Test connection
        let mut conn = conn_manager.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| MetadataError::Backend(format!("Redis ping failed: {e}")))?;

        info!("Redis connection established");
        let increment_script = Script::new(
            r#"
local key = KEYS[1]
local data = redis.call('GET', key)
if not data then
  return -1
end

local metadata = cjson.decode(data)
metadata.download_count = metadata.download_count + 1

local ttl = redis.call('TTL', key)
if ttl > 0 then
  redis.call('SET', key, cjson.encode(metadata), 'EX', ttl)
else
  redis.call('SET', key, cjson.encode(metadata))
end

return metadata.download_count
"#,
        );

        Ok(Self {
            conn_manager,
            increment_script,
        })
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
    ) -> MetadataResult<()> {
        let mut conn = self.conn_manager.clone();

        let serializable = SerializableMetadata::from(&metadata);
        let data = serde_json::to_string(&serializable)
            .map_err(|e| MetadataError::Serialization(e.to_string()))?;

        let key = self.metadata_key(token);
        let ttl_secs = ttl.as_secs() as u64;

        redis::pipe()
            .atomic()
            .set_ex(&key, data, ttl_secs)
            .zadd(Self::EXPIRY_SET_KEY, token, metadata.expires_at.timestamp())
            .query_async::<()>(&mut conn)
            .await
            .map_err(MetadataError::from)?;

        Ok(())
    }

    async fn load(&self, token: &str) -> MetadataResult<FileMetadata> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let data: String = conn.get(&key).await.map_err(|e| {
            if e.kind() == redis::ErrorKind::TypeError {
                MetadataError::NotFound
            } else {
                MetadataError::from(e)
            }
        })?;

        let serializable: SerializableMetadata = serde_json::from_str(&data)
            .map_err(|e| MetadataError::Deserialization(e.to_string()))?;

        Ok(serializable.into())
    }

    async fn increment_downloads(&self, token: &str) -> MetadataResult<i32> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let new_count: i64 = self
            .increment_script
            .key(&key)
            .invoke_async(&mut conn)
            .await
            .map_err(MetadataError::from)?;

        if new_count < 0 {
            Err(MetadataError::NotFound)
        } else {
            Ok(new_count as i32)
        }
    }

    async fn remove(&self, token: &str) -> MetadataResult<()> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        redis::pipe()
            .atomic()
            .del(&key)
            .zrem(Self::EXPIRY_SET_KEY, token)
            .query_async::<()>(&mut conn)
            .await
            .map_err(MetadataError::from)?;

        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> MetadataResult<(Vec<String>, Vec<FileMetadata>)> {
        let mut conn = self.conn_manager.clone();
        let now = Utc::now().timestamp();

        let tokens: Vec<String> = conn
            .zrangebyscore_limit(Self::EXPIRY_SET_KEY, 0, now, 0, limit as isize)
            .await
            .map_err(MetadataError::from)?;

        if tokens.is_empty() {
            return Ok((vec![], vec![]));
        }

        let mut pipe = redis::pipe();
        for token in &tokens {
            pipe.get(self.metadata_key(token));
        }

        let results: Vec<Option<String>> = pipe
            .query_async(&mut conn)
            .await
            .map_err(MetadataError::from)?;

        let mut valid_tokens = Vec::new();
        let mut metadata_list = Vec::new();

        for (token, data) in tokens.into_iter().zip(results.into_iter()) {
            if let Some(data) = data {
                if let Ok(serializable) = serde_json::from_str::<SerializableMetadata>(&data) {
                    valid_tokens.push(token);
                    metadata_list.push(serializable.into());
                }
            }
        }

        Ok((valid_tokens, metadata_list))
    }

    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: &str,
        desc: bool,
    ) -> MetadataResult<(Vec<(String, FileMetadata)>, usize)> {
        let mut conn = self.conn_manager.clone();
        let pattern = self.metadata_key("*");

        let mut all_keys = Vec::new();
        let mut cursor = 0u64;

        // Use SCAN to get all keys
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(1000)
                .query_async(&mut conn)
                .await
                .map_err(MetadataError::from)?;

            all_keys.extend(keys);
            cursor = next_cursor;

            if cursor == 0 {
                break;
            }
        }

        if all_keys.is_empty() {
            return Ok((vec![], 0));
        }

        // Fetch all metadata in parallel using pipeline
        let mut pipe = redis::pipe();
        for key in &all_keys {
            pipe.get(key);
        }

        let results: Vec<Option<String>> = pipe
            .query_async(&mut conn)
            .await
            .map_err(MetadataError::from)?;

        let mut all_files = Vec::new();
        let now = Utc::now();

        for (key, data) in all_keys.iter().zip(results.iter()) {
            if let Some(data) = data {
                if let Ok(serializable) = serde_json::from_str::<SerializableMetadata>(data) {
                    let metadata: FileMetadata = serializable.into();
                    if now < metadata.expires_at {
                        let token = key.strip_prefix("file:meta:").unwrap_or("").to_string();
                        all_files.push((token, metadata));
                    }
                }
            }
        }

        let (paginated, total) =
            sort_and_paginate_files(&mut all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
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
            download_count: meta.download_count,
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
            download_count: s.download_count,
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
        let charset = TOKEN_CHARSET.as_bytes();
        let distribution = Uniform::from(0..charset.len());
        let mut token = String::with_capacity(TOKEN_LENGTH + TOKEN_TIME_FORMAT.len());

        for _ in 0..TOKEN_LENGTH {
            let idx = rng.sample(distribution);
            token.push(char::from(charset[idx]));
        }

        token.push_str(&Utc::now().format(TOKEN_TIME_FORMAT).to_string());
        token
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

struct UploadBuffer {
    buf: Option<BytesMut>,
}

impl UploadBuffer {
    fn new() -> Self {
        let buffer = {
            let mut pool = UPLOAD_BUFFER_POOL.lock();
            pool.pop()
        }
        .unwrap_or_else(|| BytesMut::with_capacity(UPLOAD_BUFFER_SIZE));

        Self { buf: Some(buffer) }
    }

    fn len(&self) -> usize {
        self.buf.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn as_slice(&self) -> &[u8] {
        self.buf.as_ref().map(|b| b.as_ref()).unwrap_or_else(|| &[])
    }

    fn clear(&mut self) {
        if let Some(buf) = self.buf.as_mut() {
            buf.clear();
        }
    }

    fn extend_from_slice(&mut self, data: &[u8]) {
        if let Some(buf) = self.buf.as_mut() {
            buf.extend_from_slice(data);
        }
    }
}

impl Drop for UploadBuffer {
    fn drop(&mut self) {
        if let Some(mut buf) = self.buf.take() {
            if buf.capacity() <= UPLOAD_BUFFER_MAX_RETENTION {
                buf.clear();
                let mut pool = UPLOAD_BUFFER_POOL.lock();
                if pool.len() < UPLOAD_BUFFER_POOL_SIZE {
                    pool.push(buf);
                    return;
                }
            }
        }
    }
}

// ===========================
// File Server
// ===========================

#[derive(Clone)]
struct FileServer {
    config: Arc<ServerConfig>,
    store: Arc<dyn MetadataStore>,
}

#[derive(Debug)]
enum SaveFileError {
    TooLarge,
    Io(std::io::Error),
    Stream(String),
}

impl From<std::io::Error> for SaveFileError {
    fn from(value: std::io::Error) -> Self {
        SaveFileError::Io(value)
    }
}

impl FileServer {
    async fn new(config: ServerConfig) -> MetadataResult<Self> {
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

        Ok(())
    }

    async fn startup_cleanup(&self) -> Result<(), String> {
        info!("Starting cleanup of orphaned files...");
        let start_time = std::time::Instant::now();
        let mut cleaned_count = 0;
        let mut total_size = 0i64;
        let mut error_count = 0;

        let semaphore = Arc::new(Semaphore::new(STARTUP_CLEANUP_CONCURRENCY));
        let mut join_set = JoinSet::new();

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
                        let permit = semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .map_err(|e| format!("Cleanup semaphore closed: {}", e))?;
                        let server = self.clone();
                        join_set.spawn(async move {
                            let _permit = permit;
                            let result = server.check_and_clean_file(file_path).await;
                            result
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

    async fn check_and_clean_file(&self, path: PathBuf) -> Result<(bool, i64), String> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "Invalid filename".to_string())?;

        let token = if filename.len() >= 16 {
            &filename[..16]
        } else {
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
                    fs::remove_file(&path)
                        .await
                        .map_err(|e| format!("Failed to remove expired file: {}", e))?;
                    self.store.remove(token).await.map_err(|e| e.to_string())?;
                    return Ok((true, size));
                }

                if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit
                {
                    info!(
                        "Found file exceeding download limit: {:?} ({}/{})",
                        path, metadata.download_count, metadata.download_limit
                    );
                    let size = metadata.size;
                    fs::remove_file(&path)
                        .await
                        .map_err(|e| format!("Failed to remove file: {}", e))?;
                    self.store.remove(token).await.map_err(|e| e.to_string())?;
                    return Ok((true, size));
                }

                Ok((false, 0))
            }
            Err(MetadataError::NotFound) => {
                info!("Found orphaned file (no metadata): {:?}", path);
                let metadata = fs::metadata(&path)
                    .await
                    .map_err(|e| format!("Failed to get file metadata: {}", e))?;
                let size = metadata.len() as i64;
                fs::remove_file(&path)
                    .await
                    .map_err(|e| format!("Failed to remove orphaned file: {}", e))?;
                Ok((true, size))
            }
            Err(err) => {
                error!("Failed to load metadata for {}: {}", token, err);
                Err(format!("Failed to load metadata: {}", err))
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
        mut field: Field<'_>,
        filename: &str,
    ) -> Result<FileMetadata, SaveFileError> {
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

        let mut file = File::create(&full_path).await?;
        let mut total_size: u64 = 0;
        let mut buffer = UploadBuffer::new();

        loop {
            let maybe_chunk = field
                .chunk()
                .await
                .map_err(|err| SaveFileError::Stream(err.to_string()))?;

            let Some(chunk) = maybe_chunk else {
                break;
            };
            let chunk_bytes = chunk.as_ref();

            total_size = total_size
                .checked_add(chunk_bytes.len() as u64)
                .ok_or_else(|| SaveFileError::Stream("file size overflow".to_string()))?;

            if total_size > self.config.max_file_size {
                drop(file);
                if let Err(e) = fs::remove_file(&full_path).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        error!(
                            "Failed to remove oversized temp file {}: {}",
                            full_path.display(),
                            e
                        );
                    }
                }
                return Err(SaveFileError::TooLarge);
            }

            if chunk_bytes.len() >= UPLOAD_BUFFER_SIZE {
                if !buffer.is_empty() {
                    file.write_all(buffer.as_slice()).await?;
                    buffer.clear();
                }
                file.write_all(chunk_bytes).await?;
                continue;
            }

            buffer.extend_from_slice(chunk_bytes);

            if buffer.len() >= UPLOAD_BUFFER_SIZE {
                file.write_all(buffer.as_slice()).await?;
                buffer.clear();
            }
        }

        if !buffer.is_empty() {
            file.write_all(buffer.as_slice()).await?;
        }

        let now = Utc::now();
        let extension_with_dot = extension
            .as_ref()
            .map(|ext| format!(".{}", ext))
            .unwrap_or_default();

        Ok(FileMetadata {
            original_name: filename.to_string(),
            storage_path: full_path.to_string_lossy().to_string(),
            size: total_size as i64,
            extension: extension_with_dot,
            uploaded_at: now,
            expires_at: now + self.config.file_expiration,
            download_count: 0,
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

            let token = TokenGenerator::generate();
            let metadata = match server.save_file(&token, field, &filename).await {
                Ok(metadata) => metadata,
                Err(SaveFileError::TooLarge) => {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                Err(SaveFileError::Stream(e)) => {
                    warn!("Failed to read upload stream for {}: {}", filename, e);
                    return Err(StatusCode::BAD_REQUEST);
                }
                Err(SaveFileError::Io(e)) => {
                    error!("Failed to save file {}: {}", filename, e);
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            };

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

    let metadata = match server.store.load(token).await {
        Ok(metadata) => metadata,
        Err(MetadataError::NotFound) => return Err(StatusCode::NOT_FOUND),
        Err(err) => {
            error!("Failed to load metadata for {}: {}", token, err);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    if Utc::now() > metadata.expires_at {
        return Err(StatusCode::GONE);
    }

    if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit {
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

    // Open file for streaming
    let file = File::open(&metadata.storage_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Asynchronously update download count to avoid blocking the response
    let server_clone = server.clone();
    let token_owned = token.to_string();
    let metadata_clone = metadata.clone();
    tokio::spawn(async move {
        match server_clone.store.increment_downloads(&token_owned).await {
            Ok(new_count) => {
                info!(
                    "Download: {} -> {} (count: {}/{})",
                    metadata_clone.original_name,
                    token_owned,
                    new_count,
                    if metadata_clone.download_limit > 0 {
                        metadata_clone.download_limit.to_string()
                    } else {
                        "unlimited".to_string()
                    }
                );

                // Remove file if download limit reached
                if metadata_clone.download_limit > 0 && new_count >= metadata_clone.download_limit {
                    server_clone
                        .remove_file(&token_owned, &metadata_clone)
                        .await;
                }
            }
            Err(MetadataError::NotFound) => {}
            Err(err) => {
                error!(
                    "Failed to increment download count for {}: {}",
                    token_owned, err
                );
            }
        }
    });

    // Set response headers
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

    // Create streaming response
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok((headers, body))
}

async fn handle_list_files(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate API key if configured
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

    // Parse query parameters
    let page: usize = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);

    let size: usize = params
        .get("size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192)
        .min(100)
        .max(1);

    let sort_by = params
        .get("sort_by")
        .map(|s| s.as_str())
        .unwrap_or("uploaded_at");

    let order = params.get("order").map(|o| o.as_str()).unwrap_or("desc");

    let desc = order == "desc";

    // Calculate offset
    let offset = (page - 1) * size;

    // Get file list
    let (files, total) = server
        .store
        .list_files(offset, size, sort_by, desc)
        .await
        .map_err(|e| {
            error!("Failed to list files: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Build response with download links
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

    let file_items: Vec<FileItem> = files
        .into_iter()
        .map(|(token, metadata)| FileItem {
            download_link: format!(
                "{}://{}/files/{}{}",
                scheme, host, token, metadata.extension
            ),
            filename: metadata.original_name.clone(),
            size: metadata.size,
            uploaded_at: metadata.uploaded_at,
            expires_at: metadata.expires_at,
            download_count: metadata.download_count,
            token,
        })
        .collect();

    let total_pages = ((total + size - 1) / size).max(1);

    let response = ListFilesResponse {
        files: file_items,
        total,
        page,
        size,
        total_pages,
    };

    Ok(Json(response))
}

// ===========================
// Configuration Helpers
// ===========================

fn parse_duration_value(value: &str) -> Option<Duration> {
    value
        .parse::<i64>()
        .ok()
        .map(Duration::seconds)
        .or_else(|| {
            if let Some(stripped) = value.strip_suffix('h') {
                stripped.parse::<i64>().ok().map(Duration::hours)
            } else if let Some(stripped) = value.strip_suffix('d') {
                stripped.parse::<i64>().ok().map(Duration::days)
            } else if let Some(stripped) = value.strip_suffix('m') {
                stripped.parse::<i64>().ok().map(Duration::minutes)
            } else {
                None
            }
        })
}

fn parse_env_duration(key: &str, default: i64) -> Duration {
    env::var(key)
        .ok()
        .and_then(|v| parse_duration_value(&v))
        .unwrap_or_else(|| {
            info!("Using default {} seconds for {}", default, key);
            Duration::seconds(default)
        })
}

fn clamp_duration(value: Duration, min: Duration, max: Duration) -> Duration {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn resolve_cleanup_interval(file_expiration: Duration) -> Duration {
    let desired = env::var("CLEANUP_INTERVAL")
        .ok()
        .and_then(|v| parse_duration_value(&v))
        .unwrap_or(file_expiration / 2);

    let min = Duration::seconds(MIN_CLEANUP_INTERVAL_SECS);
    let max_candidate = Duration::seconds(MAX_CLEANUP_INTERVAL_SECS);

    if file_expiration < min {
        let upper = std::cmp::max(file_expiration, Duration::seconds(1));
        return clamp_duration(desired, Duration::seconds(1), upper);
    }

    let max = std::cmp::min(max_candidate, file_expiration);
    clamp_duration(desired, min, max)
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

    let max_file_size = parse_env_u64("MAX_FILE_SIZE", DEFAULT_MAX_FILE_SIZE);
    let file_expiration = parse_env_duration("FILE_EXPIRATION", DEFAULT_FILE_EXPIRATION);
    let cleanup_interval = resolve_cleanup_interval(file_expiration);

    let config = ServerConfig {
        listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string()),
        upload_dir: env::var("UPLOAD_DIR").unwrap_or_else(|_| DEFAULT_UPLOAD_DIR.to_string()),
        max_file_size,
        file_expiration,
        cleanup_interval,
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
        "Configuration: MaxFileSize={}MB, FileExpiration={}h, CleanupInterval={}m, MaxDownloads={}",
        config.max_file_size / (1024 * 1024),
        config.file_expiration.num_hours(),
        config.cleanup_interval.num_minutes(),
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
        .route("/list", get(handle_list_files))
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
