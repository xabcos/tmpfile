use super::{
    AppResult, DownloadClaim, FileSort, MetadataStore, StoredFile, TokenGenerator,
    is_valid_extension, sort_and_paginate_files,
};
use crate::error::AppError;
use crate::models::FileMetadata;
use chrono::{DateTime, Utc};
use redis::{AsyncCommands, Client, Script, aio::ConnectionManager};
use std::future::Future;
use tracing::warn;

pub struct RedisStore {
    conn_manager: ConnectionManager,
    claim_download_script: Script,
}

const REDIS_CONNECTION_RETRIES: usize = 1;
const METADATA_KEY_PREFIX: &str = "tmpfile:meta:";
const METADATA_FIELDS: [&str; 7] = [
    "original_name",
    "size",
    "extension",
    "uploaded_at",
    "expires_at",
    "download_count",
    "download_limit",
];

fn parse_dt_rfc3339(value: &str, field: &str) -> AppResult<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| AppError::InternalServerError(format!("invalid datetime field: {field}")))
}

fn parse_i64_field(value: &str, field: &str) -> AppResult<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|parsed| *parsed >= 0)
        .ok_or_else(|| AppError::InternalServerError(format!("invalid integer field: {field}")))
}

fn parse_i32_field(value: &str, field: &str) -> AppResult<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|parsed| *parsed >= 0)
        .ok_or_else(|| AppError::InternalServerError(format!("invalid integer field: {field}")))
}

fn expiry_score(expires_at: DateTime<Utc>) -> i64 {
    expires_at
        .timestamp()
        .saturating_add(i64::from(expires_at.timestamp_subsec_nanos() > 0))
}

impl RedisStore {
    pub async fn new(addr: &str, db: i64) -> AppResult<Self> {
        let client = Client::open(format!("redis://{addr}/{db}")).map_err(|e| {
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

        let claim_download_script = Script::new(
            r"
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
",
        );

        Ok(Self {
            conn_manager,
            claim_download_script,
        })
    }

    fn metadata_key(token: &str) -> String {
        format!("{METADATA_KEY_PREFIX}{token}")
    }

    fn should_retry_connection_error(err: &redis::RedisError) -> bool {
        err.is_io_error() || err.is_connection_dropped() || err.is_timeout()
    }

    async fn redis_retry<T, F, Fut>(&self, operation: &str, mut f: F) -> AppResult<T>
    where
        F: FnMut(ConnectionManager) -> Fut,
        Fut: Future<Output = redis::RedisResult<T>>,
    {
        let mut attempt = 0;

        loop {
            let conn = self.conn_manager.clone();
            match f(conn).await {
                Ok(value) => return Ok(value),
                Err(err)
                    if attempt < REDIS_CONNECTION_RETRIES
                        && Self::should_retry_connection_error(&err) =>
                {
                    attempt += 1;
                    warn!(
                        "Retrying Redis {} after connection error: {}",
                        operation, err
                    );
                }
                Err(err) => return Err(AppError::from(err)),
            }
        }
    }

    async fn save_once(
        &self,
        mut conn: ConnectionManager,
        token: &str,
        metadata: &FileMetadata,
    ) -> redis::RedisResult<()> {
        let key = Self::metadata_key(token);
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
                    ("size", size.as_str()),
                    ("extension", metadata.extension.as_str()),
                    ("uploaded_at", uploaded_at.as_str()),
                    ("expires_at", expires_at.as_str()),
                    ("download_count", download_count.as_str()),
                    ("download_limit", download_limit.as_str()),
                ],
            )
            .zadd(
                Self::EXPIRY_SET_KEY,
                token,
                expiry_score(metadata.expires_at),
            )
            .zadd(
                Self::UPLOADED_SET_KEY,
                token,
                metadata.uploaded_at.timestamp_millis(),
            )
            .query_async::<()>(&mut conn)
            .await
    }

    fn parse_hash_metadata(
        token: &str,
        values: &[Option<String>],
    ) -> AppResult<Option<FileMetadata>> {
        if TokenGenerator::parse_path(token) != Some(token) {
            return Err(AppError::InternalServerError(
                "metadata key contains an invalid token".to_string(),
            ));
        }

        if values.iter().all(Option::is_none) {
            return Ok(None);
        }

        if values.len() != METADATA_FIELDS.len() {
            return Err(AppError::InternalServerError(
                "invalid metadata hash field count".to_string(),
            ));
        }

        let get = |idx: usize, field: &'static str| -> AppResult<&str> {
            values.get(idx).and_then(|v| v.as_deref()).ok_or_else(|| {
                AppError::InternalServerError(format!(
                    "missing metadata field {field} for token {token}"
                ))
            })
        };

        let original_name = get(0, "original_name")?.to_string();
        let size = parse_i64_field(get(1, "size")?, "size")?;
        let extension = get(2, "extension")?.to_string();
        if !is_valid_extension(&extension) {
            return Err(AppError::InternalServerError(format!(
                "invalid extension field for token {token}"
            )));
        }
        let uploaded_at = parse_dt_rfc3339(get(3, "uploaded_at")?, "uploaded_at")?;
        let expires_at = parse_dt_rfc3339(get(4, "expires_at")?, "expires_at")?;
        let download_count = parse_i32_field(get(5, "download_count")?, "download_count")?;
        let download_limit = parse_i32_field(get(6, "download_limit")?, "download_limit")?;

        Ok(Some(FileMetadata {
            original_name,
            size,
            extension,
            uploaded_at,
            expires_at,
            download_count,
            download_limit,
        }))
    }

    async fn load_metadata_from_hash(&self, token: &str) -> AppResult<Option<FileMetadata>> {
        let key = Self::metadata_key(token);

        let values: Vec<Option<String>> = self
            .redis_retry("metadata load", |mut conn| {
                let key = key.clone();
                async move { conn.hmget(&key, &METADATA_FIELDS).await }
            })
            .await?;
        Self::parse_hash_metadata(token, &values)
    }

    fn queue_metadata_hmget(pipe: &mut redis::Pipeline, token: &str) {
        pipe.hmget(Self::metadata_key(token), &METADATA_FIELDS);
    }

    async fn remove_index_entries(&self, operation: &str, tokens: &[String]) -> AppResult<()> {
        if tokens.is_empty() {
            return Ok(());
        }

        let tokens = tokens.to_vec();
        self.redis_retry(operation, |mut conn| {
            let tokens = tokens.clone();
            async move {
                let mut pipe = redis::pipe();
                pipe.atomic();
                for token in tokens {
                    pipe.zrem(Self::EXPIRY_SET_KEY, &token)
                        .zrem(Self::UPLOADED_SET_KEY, &token);
                }
                pipe.query_async::<()>(&mut conn).await
            }
        })
        .await
    }

    const EXPIRY_SET_KEY: &'static str = "tmpfile:expiry";
    const UPLOADED_SET_KEY: &'static str = "tmpfile:uploaded";

    async fn list_files_by_uploaded_at(
        &self,
        offset: usize,
        limit: usize,
        desc: bool,
    ) -> AppResult<(Vec<StoredFile>, usize)> {
        let now = Utc::now();

        let mut files = Vec::with_capacity(limit);
        let mut active_seen = 0usize;
        let mut total_active = 0usize;
        let mut zset_index = 0usize;
        let mut stale_tokens = Vec::new();

        let chunk_size = std::cmp::max(limit.saturating_mul(8), 256);

        loop {
            let start = isize::try_from(zset_index)
                .map_err(|_| AppError::BadRequest("page too large".to_string()))?;
            let stop = isize::try_from(zset_index.saturating_add(chunk_size).saturating_sub(1))
                .map_err(|_| AppError::BadRequest("page too large".to_string()))?;

            let tokens: Vec<String> = self
                .redis_retry("uploaded set range", |mut conn| async move {
                    if desc {
                        conn.zrevrange(Self::UPLOADED_SET_KEY, start, stop).await
                    } else {
                        conn.zrange(Self::UPLOADED_SET_KEY, start, stop).await
                    }
                })
                .await?;

            if tokens.is_empty() {
                break;
            }

            let batch_len = tokens.len();

            let results: Vec<Vec<Option<String>>> = self
                .redis_retry("metadata batch load", |mut conn| {
                    let mut pipe = redis::pipe();
                    for token in &tokens {
                        Self::queue_metadata_hmget(&mut pipe, token);
                    }
                    async move { pipe.query_async(&mut conn).await }
                })
                .await?;

            for (token, values) in tokens.into_iter().zip(results) {
                let metadata = match Self::parse_hash_metadata(&token, &values) {
                    Ok(Some(metadata)) => metadata,
                    Ok(None) => {
                        stale_tokens.push(token);
                        continue;
                    }
                    Err(err) => {
                        warn!("Ignoring corrupt metadata for token {}: {}", token, err);
                        stale_tokens.push(token);
                        continue;
                    }
                };

                if now >= metadata.expires_at {
                    continue;
                }

                total_active += 1;

                if active_seen < offset {
                    active_seen += 1;
                    continue;
                }

                if files.len() < limit {
                    files.push((token, metadata));
                }
            }

            zset_index = zset_index.saturating_add(batch_len);
        }

        // Mutating a sorted set while scanning it by numeric index can skip
        // entries that shift left. Repair stale members only after the scan.
        self.remove_index_entries("stale uploaded-set cleanup", &stale_tokens)
            .await?;

        Ok((files, total_active))
    }
}

#[async_trait::async_trait]
impl MetadataStore for RedisStore {
    async fn save(&self, token: &str, metadata: FileMetadata) -> AppResult<()> {
        self.redis_retry("metadata save", |conn| {
            self.save_once(conn, token, &metadata)
        })
        .await
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        self.load_metadata_from_hash(token)
            .await?
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))
    }

    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim> {
        let mut conn = self.conn_manager.clone();
        let key = Self::metadata_key(token);

        let result: i64 = self
            .claim_download_script
            .key(key)
            .invoke_async(&mut conn)
            .await
            .map_err(AppError::from)?;

        match result {
            -1 => Err(AppError::NotFound("file not found".to_string())),
            -2 => Ok(DownloadClaim::LimitReached),
            count if count >= 0 => i32::try_from(count)
                .map(|new_count| DownloadClaim::Allowed { new_count })
                .map_err(|_| {
                    AppError::InternalServerError("download count exceeds supported range".into())
                }),
            _ => Err(AppError::InternalServerError(
                "invalid claim_download result".to_string(),
            )),
        }
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let key = Self::metadata_key(token);

        self.redis_retry("metadata remove", |mut conn| {
            let mut pipe = redis::pipe();
            pipe.atomic()
                .del(&key)
                .zrem(Self::EXPIRY_SET_KEY, token)
                .zrem(Self::UPLOADED_SET_KEY, token);
            async move { pipe.query_async::<()>(&mut conn).await }
        })
        .await
    }

    async fn list_expired(&self, limit: usize) -> AppResult<Vec<StoredFile>> {
        let limit = isize::try_from(limit)
            .map_err(|_| AppError::InternalServerError("cleanup batch is too large".into()))?;
        let now_score = Utc::now().timestamp();

        let tokens: Vec<String> = self
            .redis_retry("expired set range", |mut conn| async move {
                conn.zrangebyscore_limit(Self::EXPIRY_SET_KEY, 0, now_score, 0, limit)
                    .await
            })
            .await?;

        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let results: Vec<Vec<Option<String>>> = self
            .redis_retry("expired metadata batch load", |mut conn| {
                let mut pipe = redis::pipe();
                for token in &tokens {
                    Self::queue_metadata_hmget(&mut pipe, token);
                }
                async move { pipe.query_async(&mut conn).await }
            })
            .await?;

        let mut expired = Vec::new();
        let mut stale_tokens = Vec::new();

        for (token, values) in tokens.into_iter().zip(results) {
            match Self::parse_hash_metadata(&token, &values) {
                Ok(Some(metadata)) => expired.push((token, metadata)),
                Ok(None) => stale_tokens.push(token),
                Err(err) => {
                    warn!("Removing corrupt indexes for token {}: {}", token, err);
                    stale_tokens.push(token);
                }
            }
        }

        if !stale_tokens.is_empty() {
            self.remove_index_entries("stale expiry index cleanup", &stale_tokens)
                .await?;
        }

        Ok(expired)
    }

    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: FileSort,
        desc: bool,
    ) -> AppResult<(Vec<StoredFile>, usize)> {
        if sort_by == FileSort::UploadedAt {
            let uploaded_count: i64 = self
                .redis_retry("uploaded set count", |mut conn| async move {
                    conn.zcard(Self::UPLOADED_SET_KEY).await
                })
                .await?;
            let expiry_count: i64 = self
                .redis_retry("expiry set count", |mut conn| async move {
                    conn.zcard(Self::EXPIRY_SET_KEY).await
                })
                .await?;

            if uploaded_count > 0 && uploaded_count == expiry_count {
                return self.list_files_by_uploaded_at(offset, limit, desc).await;
            }
        }

        let pattern = Self::metadata_key("*");
        let mut cursor = 0u64;
        let mut all_files = Vec::new();
        let mut stale_tokens = Vec::new();
        let now = Utc::now();

        loop {
            let (next_cursor, keys): (u64, Vec<String>) = self
                .redis_retry("metadata scan", |mut conn| {
                    let pattern = pattern.clone();
                    async move {
                        redis::cmd("SCAN")
                            .arg(cursor)
                            .arg("MATCH")
                            .arg(&pattern)
                            .arg("COUNT")
                            .arg(1000)
                            .query_async(&mut conn)
                            .await
                    }
                })
                .await?;

            if !keys.is_empty() {
                let results: Vec<Vec<Option<String>>> = self
                    .redis_retry("listed metadata batch load", |mut conn| {
                        let mut pipe = redis::pipe();
                        for key in &keys {
                            pipe.hmget(key, &METADATA_FIELDS);
                        }
                        async move { pipe.query_async(&mut conn).await }
                    })
                    .await?;

                for (key, values) in keys.iter().zip(results) {
                    let Some(token) = key.strip_prefix(METADATA_KEY_PREFIX) else {
                        continue;
                    };
                    match Self::parse_hash_metadata(token, &values) {
                        Ok(Some(metadata)) if now < metadata.expires_at => {
                            all_files.push((token.to_string(), metadata));
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => stale_tokens.push(token.to_string()),
                        Err(err) => {
                            warn!("Ignoring corrupt metadata for token {}: {}", token, err);
                            stale_tokens.push(token.to_string());
                        }
                    }
                }
            }

            cursor = next_cursor;

            if cursor == 0 {
                break;
            }
        }

        self.remove_index_entries("stale listed-set cleanup", &stale_tokens)
            .await?;

        let (paginated, total) = sort_and_paginate_files(all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::sync::Arc;

    const VALID_TOKEN: &str = "0123456789abcdefghijklmno";

    fn metadata(name: &str, expires_at: DateTime<Utc>, download_limit: i32) -> FileMetadata {
        FileMetadata {
            original_name: name.to_string(),
            size: 42,
            extension: "txt".to_string(),
            uploaded_at: Utc::now(),
            expires_at,
            download_count: 0,
            download_limit,
        }
    }

    #[test]
    fn hash_metadata_parser_rejects_missing_and_invalid_fields() {
        let now = Utc::now();
        let values = vec![
            Some("example.txt".to_string()),
            Some("42".to_string()),
            Some("txt".to_string()),
            Some(now.to_rfc3339()),
            Some((now + Duration::hours(1)).to_rfc3339()),
            Some("0".to_string()),
            Some("3".to_string()),
        ];
        let parsed = RedisStore::parse_hash_metadata(VALID_TOKEN, &values)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.original_name, "example.txt");
        assert_eq!(parsed.download_limit, 3);

        let mut invalid = values.clone();
        invalid[1] = Some("not-a-size".to_string());
        assert!(RedisStore::parse_hash_metadata(VALID_TOKEN, &invalid).is_err());
        invalid = values.clone();
        invalid[2] = Some(".txt".to_string());
        assert!(RedisStore::parse_hash_metadata(VALID_TOKEN, &invalid).is_err());
        invalid[2] = Some("TXT".to_string());
        assert!(RedisStore::parse_hash_metadata(VALID_TOKEN, &invalid).is_err());
        assert_eq!(
            RedisStore::parse_hash_metadata(VALID_TOKEN, &vec![None; METADATA_FIELDS.len()])
                .unwrap()
                .map(|metadata| metadata.original_name),
            None
        );
        assert!(RedisStore::parse_hash_metadata("../outside", &values).is_err());
    }

    #[tokio::test]
    #[ignore = "requires TEST_REDIS_ADDR pointing to an isolated Redis instance"]
    async fn redis_store_contract() {
        let addr = std::env::var("TEST_REDIS_ADDR").expect("TEST_REDIS_ADDR is required");
        let db = std::env::var("TEST_REDIS_DB")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let store = Arc::new(RedisStore::new(&addr, db).await.unwrap());
        let limited_token = crate::storage::TokenGenerator::generate();
        let expiring_token = crate::storage::TokenGenerator::generate();
        let corrupt_token = crate::storage::TokenGenerator::generate();
        let expired_after_corrupt = crate::storage::TokenGenerator::generate();

        store
            .save(
                &limited_token,
                metadata("limited.txt", Utc::now() + Duration::hours(1), 5),
            )
            .await
            .unwrap();
        assert_eq!(
            store.load(&limited_token).await.unwrap().original_name,
            "limited.txt"
        );

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let store = Arc::clone(&store);
            let token = limited_token.clone();
            tasks.spawn(async move { store.claim_download(&token).await.unwrap() });
        }
        let mut allowed = 0;
        while let Some(result) = tasks.join_next().await {
            if matches!(result.unwrap(), DownloadClaim::Allowed { .. }) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 5);

        let mut conn = store.conn_manager.clone();
        let metadata_key = RedisStore::metadata_key(&limited_token);
        let field_count: usize = redis::cmd("HLEN")
            .arg(&metadata_key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(field_count, METADATA_FIELDS.len());
        let ttl: i64 = redis::cmd("TTL")
            .arg(&metadata_key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(ttl, -1, "cleanup metadata must not expire before the file");

        store
            .save(
                &corrupt_token,
                metadata("corrupt.txt", Utc::now() + Duration::hours(1), 0),
            )
            .await
            .unwrap();
        store
            .save(
                &expired_after_corrupt,
                metadata("expired.txt", Utc::now() - Duration::seconds(1), 0),
            )
            .await
            .unwrap();
        redis::pipe()
            .hset(
                RedisStore::metadata_key(&corrupt_token),
                "size",
                "not-an-integer",
            )
            .zadd(RedisStore::EXPIRY_SET_KEY, &corrupt_token, 0)
            .query_async::<()>(&mut conn)
            .await
            .unwrap();

        let expired = store.list_expired(1024).await.unwrap();
        assert!(
            expired
                .iter()
                .any(|(token, _)| token == &expired_after_corrupt),
            "one corrupt hash must not starve later expired files"
        );
        let corrupt_score: Option<f64> = conn
            .zscore(RedisStore::EXPIRY_SET_KEY, &corrupt_token)
            .await
            .unwrap();
        assert_eq!(corrupt_score, None);

        store
            .save(
                &expiring_token,
                metadata("expiring.txt", Utc::now() + Duration::milliseconds(300), 0),
            )
            .await
            .unwrap();
        assert!(
            store
                .list_expired(1024)
                .await
                .unwrap()
                .iter()
                .all(|(token, _)| token != &expiring_token)
        );
        // The expiry score is rounded up to whole seconds, so cleanup may
        // intentionally lag the full timestamp by less than one second.
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        assert!(
            store
                .list_expired(1024)
                .await
                .unwrap()
                .iter()
                .any(|(token, _)| token == &expiring_token)
        );

        store.remove(&limited_token).await.unwrap();
        store.remove(&expiring_token).await.unwrap();
        store.remove(&corrupt_token).await.unwrap();
        store.remove(&expired_after_corrupt).await.unwrap();
    }
}
