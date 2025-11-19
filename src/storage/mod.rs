use crate::config::{
    CLEANUP_BATCH_SIZE, FILE_WRITE_BUFFER_SIZE, MEMORY_SHARD_COUNT, STARTUP_CLEANUP_CONCURRENCY,
    STORAGE_SUBDIR_DEPTH, ServerConfig, TOKEN_CHARSET, TOKEN_LENGTH, TOKEN_TIME_FORMAT,
};
use crate::error::AppError;
use crate::models::{FileMetadata, SerializableMetadata};
use axum::extract::multipart::Field;
use chrono::{Duration, Utc};
use parking_lot::RwLock;
use rand::Rng;
use redis::{AsyncCommands, Client, Script, aio::ConnectionManager};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::interval;
use tracing::{error, info};

type AppResult<T> = Result<T, AppError>;

// ===========================
// Token Generation
// ===========================

pub struct TokenGenerator;

impl TokenGenerator {
    pub fn generate() -> String {
        let mut rng = rand::rng();
        let charset = TOKEN_CHARSET.as_bytes();
        let mut token = String::with_capacity(TOKEN_LENGTH + TOKEN_TIME_FORMAT.len());

        for _ in 0..TOKEN_LENGTH {
            let idx = rng.random_range(0..charset.len());
            token.push(char::from(charset[idx]));
        }

        token.push_str(&Utc::now().format(TOKEN_TIME_FORMAT).to_string());
        token
    }
}

// ===========================
// Storage Trait
// ===========================

#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    async fn save(&self, token: &str, metadata: FileMetadata, ttl: StdDuration) -> AppResult<()>;
    async fn load(&self, token: &str) -> AppResult<FileMetadata>;
    async fn increment_downloads(&self, token: &str) -> AppResult<i32>;
    async fn remove(&self, token: &str) -> AppResult<()>;
    async fn list_expired(&self, limit: usize) -> AppResult<(Vec<String>, Vec<FileMetadata>)>;
    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: &str,
        desc: bool,
    ) -> AppResult<(Vec<(String, FileMetadata)>, usize)>;
}

fn sort_and_paginate_files(
    files: &mut [(String, FileMetadata)],
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

pub struct MemoryStore {
    shards: Vec<MemoryShard>,
}

impl MemoryStore {
    pub fn new() -> Self {
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
    ) -> AppResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        let ttl_duration =
            Duration::from_std(ttl).map_err(|e| AppError::InternalServerError(e.to_string()))?;
        metadata.expires_at = Utc::now() + ttl_duration;
        guard.insert(token.to_string(), metadata);
        Ok(())
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read();
        guard
            .get(token)
            .cloned()
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))
    }

    async fn increment_downloads(&self, token: &str) -> AppResult<i32> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        let metadata = guard
            .get_mut(token)
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))?;
        metadata.download_count += 1;
        Ok(metadata.download_count)
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write();
        guard.remove(token);
        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> AppResult<(Vec<String>, Vec<FileMetadata>)> {
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
    ) -> AppResult<(Vec<(String, FileMetadata)>, usize)> {
        let mut all_files = Vec::new();
        let now = Utc::now();

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

pub struct RedisStore {
    conn_manager: ConnectionManager,
    increment_script: Script,
}

impl RedisStore {
    pub async fn new(addr: &str, db: i64) -> AppResult<Self> {
        let client = Client::open(format!("redis://{}/{}", addr, db)).map_err(|e| {
            AppError::InternalServerError(format!("Failed to create Redis client: {e}"))
        })?;

        let conn_manager = ConnectionManager::new(client)
            .await
            .map_err(|e| AppError::InternalServerError(format!("Redis connection failed: {e}")))?;

        // Test connection
        let mut conn = conn_manager.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| AppError::InternalServerError(format!("Redis ping failed: {e}")))?;

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
    async fn save(&self, token: &str, metadata: FileMetadata, ttl: StdDuration) -> AppResult<()> {
        let mut conn = self.conn_manager.clone();

        let serializable = SerializableMetadata::from(&metadata);
        let data = serde_json::to_string(&serializable)?;

        let key = self.metadata_key(token);
        let ttl_secs = ttl.as_secs();

        redis::pipe()
            .atomic()
            .set_ex(&key, data, ttl_secs)
            .zadd(Self::EXPIRY_SET_KEY, token, metadata.expires_at.timestamp())
            .query_async::<()>(&mut conn)
            .await
            .map_err(AppError::from)?;

        Ok(())
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let data: String = conn.get(&key).await.map_err(|e| {
            if e.kind() == redis::ErrorKind::TypeError {
                AppError::NotFound("file not found".to_string())
            } else {
                AppError::from(e)
            }
        })?;

        let serializable: SerializableMetadata = serde_json::from_str(&data)?;
        Ok(serializable.into())
    }

    async fn increment_downloads(&self, token: &str) -> AppResult<i32> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let new_count: i64 = self
            .increment_script
            .key(&key)
            .invoke_async(&mut conn)
            .await
            .map_err(AppError::from)?;

        if new_count < 0 {
            Err(AppError::NotFound("file not found".to_string()))
        } else {
            Ok(new_count as i32)
        }
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        redis::pipe()
            .atomic()
            .del(&key)
            .zrem(Self::EXPIRY_SET_KEY, token)
            .query_async::<()>(&mut conn)
            .await
            .map_err(AppError::from)?;

        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> AppResult<(Vec<String>, Vec<FileMetadata>)> {
        let mut conn = self.conn_manager.clone();
        let now = Utc::now().timestamp();

        let tokens: Vec<String> = conn
            .zrangebyscore_limit(Self::EXPIRY_SET_KEY, 0, now, 0, limit as isize)
            .await
            .map_err(AppError::from)?;

        if tokens.is_empty() {
            return Ok((vec![], vec![]));
        }

        let mut pipe = redis::pipe();
        for token in &tokens {
            pipe.get(self.metadata_key(token));
        }

        let results: Vec<Option<String>> =
            pipe.query_async(&mut conn).await.map_err(AppError::from)?;

        let mut valid_tokens = Vec::new();
        let mut metadata_list = Vec::new();

        for (token, data) in tokens.into_iter().zip(results.into_iter()) {
            if let Some(data) = data
                && let Ok(serializable) = serde_json::from_str::<SerializableMetadata>(&data)
            {
                valid_tokens.push(token);
                metadata_list.push(serializable.into());
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
    ) -> AppResult<(Vec<(String, FileMetadata)>, usize)> {
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
                .map_err(AppError::from)?;

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

        let results: Vec<Option<String>> =
            pipe.query_async(&mut conn).await.map_err(AppError::from)?;

        let mut all_files = Vec::new();
        let now = Utc::now();

        for (key, data) in all_keys.iter().zip(results.iter()) {
            if let Some(data) = data
                && let Ok(serializable) = serde_json::from_str::<SerializableMetadata>(data)
            {
                let metadata: FileMetadata = serializable.into();
                if now < metadata.expires_at {
                    let token = key.strip_prefix("file:meta:").unwrap_or("").to_string();
                    all_files.push((token, metadata));
                }
            }
        }

        let (paginated, total) =
            sort_and_paginate_files(&mut all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
    }
}

// ===========================
// File Server
// ===========================

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

    pub async fn initialize_storage(&self) -> Result<(), String> {
        fs::create_dir_all(&self.config.upload_dir)
            .await
            .map_err(|e| format!("Failed to create upload directory: {}", e))?;

        Ok(())
    }

    pub async fn startup_cleanup(&self) -> Result<(), String> {
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
            Err(AppError::NotFound(_)) => {
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

    pub fn get_storage_path(&self, token: &str) -> PathBuf {
        let subdir = &token[..STORAGE_SUBDIR_DEPTH];
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
            let maybe_chunk = field
                .chunk()
                .await
                .map_err(|err| AppError::BadRequest(format!("Stream error: {}", err)))?;

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
