use super::{AppResult, DownloadClaim, MetadataStore, sort_and_paginate_files};
use crate::config::MEMORY_SHARD_COUNT;
use crate::error::AppError;
use crate::models::FileMetadata;
use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration as StdDuration;

type MemoryShard = Arc<RwLock<HashMap<String, FileMetadata>>>;

pub struct MemoryStore {
    shards: Vec<MemoryShard>,
}

fn map_lock_poison<T>(_: PoisonError<T>) -> AppError {
    AppError::InternalServerError("memory store lock poisoned".to_string())
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
        let mut guard = shard.write().map_err(map_lock_poison)?;
        let ttl_duration =
            Duration::from_std(ttl).map_err(|e| AppError::InternalServerError(e.to_string()))?;
        metadata.expires_at = Utc::now() + ttl_duration;
        guard.insert(token.to_string(), metadata);
        Ok(())
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read().map_err(map_lock_poison)?;
        guard
            .get(token)
            .cloned()
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))
    }

    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write().map_err(map_lock_poison)?;
        let metadata = guard
            .get_mut(token)
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))?;

        if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit {
            return Ok(DownloadClaim::LimitReached);
        }

        metadata.download_count += 1;
        Ok(DownloadClaim::Allowed {
            new_count: metadata.download_count,
        })
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write().map_err(map_lock_poison)?;
        guard.remove(token);
        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> AppResult<(Vec<String>, Vec<FileMetadata>)> {
        let mut tokens = Vec::new();
        let mut metadata = Vec::new();
        let now = Utc::now();

        for shard in &self.shards {
            let guard = shard.read().map_err(map_lock_poison)?;
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
            let guard = shard.read().map_err(map_lock_poison)?;
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
