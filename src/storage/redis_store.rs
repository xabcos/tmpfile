use super::{AppResult, DownloadClaim, MetadataStore, sort_and_paginate_files};
use crate::error::AppError;
use crate::models::FileMetadata;
use chrono::{DateTime, Utc};
use redis::{AsyncCommands, Client, Script, aio::ConnectionManager};
use std::time::Duration as StdDuration;
use tracing::info;

pub struct RedisStore {
    conn_manager: ConnectionManager,
    claim_download_script: Script,
}

fn parse_dt_rfc3339(value: &str, field: &str) -> AppResult<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| AppError::InternalServerError(format!("invalid datetime field: {}", field)))
}

fn parse_i64_field(value: &str, field: &str) -> AppResult<i64> {
    value
        .parse::<i64>()
        .map_err(|_| AppError::InternalServerError(format!("invalid integer field: {}", field)))
}

fn parse_i32_field(value: &str, field: &str) -> AppResult<i32> {
    value
        .parse::<i32>()
        .map_err(|_| AppError::InternalServerError(format!("invalid integer field: {}", field)))
}

impl RedisStore {
    pub async fn new(addr: &str, db: i64) -> AppResult<Self> {
        let client = Client::open(format!("redis://{}/{}", addr, db)).map_err(|e| {
            AppError::InternalServerError(format!("Failed to create Redis client: {e}"))
        })?;

        let conn_manager = ConnectionManager::new(client)
            .await
            .map_err(|e| AppError::InternalServerError(format!("Redis connection failed: {e}")))?;

        let mut conn = conn_manager.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| AppError::InternalServerError(format!("Redis ping failed: {e}")))?;

        info!("Redis connection established");
        let claim_download_script = Script::new(
            r#"
local key = KEYS[1]

if redis.call('EXISTS', key) == 0 then
  return -1
end

local limit = tonumber(redis.call('HGET', key, 'download_limit') or '0')
local count = tonumber(redis.call('HGET', key, 'download_count') or '0')

if limit > 0 and count >= limit then
  return -2
end

local new_count = redis.call('HINCRBY', key, 'download_count', 1)
return new_count
"#,
        );

        Ok(Self {
            conn_manager,
            claim_download_script,
        })
    }

    fn metadata_key(&self, token: &str) -> String {
        format!("file:meta:{}", token)
    }

    fn parse_hash_metadata(
        token: &str,
        values: Vec<Option<String>>,
    ) -> AppResult<Option<FileMetadata>> {
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }

        if values.len() != 8 {
            return Err(AppError::InternalServerError(
                "invalid metadata hash field count".to_string(),
            ));
        }

        let get = |idx: usize, field: &'static str| -> AppResult<&str> {
            values.get(idx).and_then(|v| v.as_deref()).ok_or_else(|| {
                AppError::InternalServerError(format!(
                    "missing metadata field {} for token {}",
                    field, token
                ))
            })
        };

        let original_name = get(0, "original_name")?.to_string();
        let storage_path = get(1, "storage_path")?.to_string();
        let size = parse_i64_field(get(2, "size")?, "size")?;
        let extension = get(3, "extension")?.to_string();
        let uploaded_at = parse_dt_rfc3339(get(4, "uploaded_at")?, "uploaded_at")?;
        let expires_at = parse_dt_rfc3339(get(5, "expires_at")?, "expires_at")?;
        let download_count = parse_i32_field(get(6, "download_count")?, "download_count")?;
        let download_limit = parse_i32_field(get(7, "download_limit")?, "download_limit")?;

        Ok(Some(FileMetadata {
            original_name,
            storage_path,
            size,
            extension,
            uploaded_at,
            expires_at,
            download_count,
            download_limit,
        }))
    }

    async fn load_metadata_from_hash(
        &self,
        conn: &mut ConnectionManager,
        token: &str,
    ) -> AppResult<Option<FileMetadata>> {
        let key = self.metadata_key(token);
        let fields = [
            "original_name",
            "storage_path",
            "size",
            "extension",
            "uploaded_at",
            "expires_at",
            "download_count",
            "download_limit",
        ];

        let values: Vec<Option<String>> =
            conn.hmget(&key, &fields).await.map_err(AppError::from)?;
        Self::parse_hash_metadata(token, values)
    }

    const EXPIRY_SET_KEY: &'static str = "file:expiry:set";
    const UPLOADED_SET_KEY: &'static str = "file:uploaded:set";

    async fn list_files_by_uploaded_at(
        &self,
        conn: &mut ConnectionManager,
        offset: usize,
        limit: usize,
        desc: bool,
    ) -> AppResult<(Vec<(String, FileMetadata)>, usize)> {
        let now = Utc::now();
        let now_ts = now.timestamp();

        let total: i64 = conn
            .zcount(Self::EXPIRY_SET_KEY, format!("({}", now_ts), "+inf")
            .await
            .map_err(AppError::from)?;

        if total <= 0 {
            return Ok((vec![], 0));
        }

        let mut files = Vec::with_capacity(limit);
        let mut active_seen = 0usize;
        let mut zset_index = 0usize;

        let chunk_size = std::cmp::max(limit.saturating_mul(8), 256);

        while files.len() < limit {
            let start = isize::try_from(zset_index)
                .map_err(|_| AppError::BadRequest("page too large".to_string()))?;
            let stop = isize::try_from(zset_index.saturating_add(chunk_size).saturating_sub(1))
                .map_err(|_| AppError::BadRequest("page too large".to_string()))?;

            let tokens: Vec<String> = if desc {
                conn.zrevrange(Self::UPLOADED_SET_KEY, start, stop)
                    .await
                    .map_err(AppError::from)?
            } else {
                conn.zrange(Self::UPLOADED_SET_KEY, start, stop)
                    .await
                    .map_err(AppError::from)?
            };

            if tokens.is_empty() {
                break;
            }

            let batch_len = tokens.len();

            let mut pipe = redis::pipe();
            for token in &tokens {
                pipe.hmget(
                    self.metadata_key(token),
                    &[
                        "original_name",
                        "storage_path",
                        "size",
                        "extension",
                        "uploaded_at",
                        "expires_at",
                        "download_count",
                        "download_limit",
                    ],
                );
            }

            let results: Vec<Vec<Option<String>>> =
                pipe.query_async(conn).await.map_err(AppError::from)?;

            for (token, values) in tokens.into_iter().zip(results.into_iter()) {
                let Some(metadata) = Self::parse_hash_metadata(&token, values)? else {
                    redis::pipe()
                        .atomic()
                        .zrem(Self::EXPIRY_SET_KEY, &token)
                        .zrem(Self::UPLOADED_SET_KEY, &token)
                        .query_async::<()>(conn)
                        .await
                        .map_err(AppError::from)?;
                    continue;
                };

                if now >= metadata.expires_at {
                    continue;
                }

                if active_seen < offset {
                    active_seen += 1;
                    continue;
                }

                files.push((token, metadata));
                if files.len() >= limit {
                    break;
                }
            }

            zset_index = zset_index.saturating_add(batch_len);
        }

        Ok((files, total as usize))
    }
}

#[async_trait::async_trait]
impl MetadataStore for RedisStore {
    async fn save(&self, token: &str, metadata: FileMetadata, ttl: StdDuration) -> AppResult<()> {
        let mut conn = self.conn_manager.clone();

        let key = self.metadata_key(token);
        let ttl_secs = ttl.as_secs();
        let size = metadata.size.to_string();
        let download_count = metadata.download_count.to_string();
        let download_limit = metadata.download_limit.to_string();
        let uploaded_at = metadata.uploaded_at.to_rfc3339();
        let expires_at = metadata.expires_at.to_rfc3339();

        redis::pipe()
            .atomic()
            .hset_multiple(
                &key,
                &[
                    ("original_name", metadata.original_name.as_str()),
                    ("storage_path", metadata.storage_path.as_str()),
                    ("size", size.as_str()),
                    ("extension", metadata.extension.as_str()),
                    ("uploaded_at", uploaded_at.as_str()),
                    ("expires_at", expires_at.as_str()),
                    ("download_count", download_count.as_str()),
                    ("download_limit", download_limit.as_str()),
                ],
            )
            .expire(&key, ttl_secs as i64)
            .zadd(Self::EXPIRY_SET_KEY, token, metadata.expires_at.timestamp())
            .zadd(
                Self::UPLOADED_SET_KEY,
                token,
                metadata.uploaded_at.timestamp(),
            )
            .query_async::<()>(&mut conn)
            .await
            .map_err(AppError::from)?;

        Ok(())
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        let mut conn = self.conn_manager.clone();
        self.load_metadata_from_hash(&mut conn, token)
            .await?
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))
    }

    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        let result: i64 = self
            .claim_download_script
            .key(&key)
            .invoke_async(&mut conn)
            .await
            .map_err(AppError::from)?;

        match result {
            -1 => Err(AppError::NotFound("file not found".to_string())),
            -2 => Ok(DownloadClaim::LimitReached),
            count if count >= 0 => Ok(DownloadClaim::Allowed {
                new_count: count as i32,
            }),
            _ => Err(AppError::InternalServerError(
                "invalid claim_download result".to_string(),
            )),
        }
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let mut conn = self.conn_manager.clone();
        let key = self.metadata_key(token);

        redis::pipe()
            .atomic()
            .del(&key)
            .zrem(Self::EXPIRY_SET_KEY, token)
            .zrem(Self::UPLOADED_SET_KEY, token)
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
            pipe.hmget(
                self.metadata_key(token),
                &[
                    "original_name",
                    "storage_path",
                    "size",
                    "extension",
                    "uploaded_at",
                    "expires_at",
                    "download_count",
                    "download_limit",
                ],
            );
        }

        let results: Vec<Vec<Option<String>>> =
            pipe.query_async(&mut conn).await.map_err(AppError::from)?;

        let mut valid_tokens = Vec::new();
        let mut metadata_list = Vec::new();

        for (token, values) in tokens.into_iter().zip(results.into_iter()) {
            if let Some(metadata) = Self::parse_hash_metadata(&token, values)? {
                valid_tokens.push(token);
                metadata_list.push(metadata);
            } else {
                redis::pipe()
                    .atomic()
                    .zrem(Self::EXPIRY_SET_KEY, &token)
                    .zrem(Self::UPLOADED_SET_KEY, &token)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(AppError::from)?;
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

        if sort_by == "uploaded_at" {
            let uploaded_count: i64 = conn
                .zcard(Self::UPLOADED_SET_KEY)
                .await
                .map_err(AppError::from)?;
            let expiry_count: i64 = conn
                .zcard(Self::EXPIRY_SET_KEY)
                .await
                .map_err(AppError::from)?;

            if uploaded_count > 0 && uploaded_count == expiry_count {
                return self
                    .list_files_by_uploaded_at(&mut conn, offset, limit, desc)
                    .await;
            }
        }

        let pattern = self.metadata_key("*");

        let mut all_keys = Vec::new();
        let mut cursor = 0u64;

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

        let mut pipe = redis::pipe();
        for key in &all_keys {
            pipe.hmget(
                key,
                &[
                    "original_name",
                    "storage_path",
                    "size",
                    "extension",
                    "uploaded_at",
                    "expires_at",
                    "download_count",
                    "download_limit",
                ],
            );
        }

        let results: Vec<Vec<Option<String>>> =
            pipe.query_async(&mut conn).await.map_err(AppError::from)?;

        let mut all_files = Vec::new();
        let now = Utc::now();

        for (key, values) in all_keys.iter().zip(results.into_iter()) {
            let token = key.strip_prefix("file:meta:").unwrap_or("");
            if let Some(metadata) = Self::parse_hash_metadata(token, values)? {
                if now < metadata.expires_at {
                    all_files.push((token.to_string(), metadata));
                }
            } else {
                redis::pipe()
                    .atomic()
                    .zrem(Self::EXPIRY_SET_KEY, token)
                    .zrem(Self::UPLOADED_SET_KEY, token)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(AppError::from)?;
            }
        }

        let (paginated, total) =
            sort_and_paginate_files(&mut all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
    }
}
