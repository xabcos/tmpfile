use super::{
    AppResult, DownloadClaim, FileSort, MetadataStore, StoredFile, sort_and_paginate_files,
};
use crate::config::MEMORY_SHARD_COUNT;
use crate::error::AppError;
use crate::models::FileMetadata;
use chrono::Utc;
use std::collections::HashMap;
use tokio::sync::RwLock;

type MemoryShard = RwLock<HashMap<String, FileMetadata>>;

pub struct MemoryStore {
    shards: Vec<MemoryShard>,
}

impl MemoryStore {
    pub fn new() -> Self {
        let shards = (0..MEMORY_SHARD_COUNT)
            .map(|_| RwLock::new(HashMap::new()))
            .collect();
        Self { shards }
    }

    fn get_shard_for_token(&self, token: &str) -> &MemoryShard {
        let hash = token.bytes().fold(0u64, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(u64::from(byte))
        });
        let shard_count = u64::try_from(self.shards.len()).expect("shard count fits in u64");
        let index = usize::try_from(hash % shard_count).expect("shard index fits in usize");
        &self.shards[index]
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl MetadataStore for MemoryStore {
    async fn save(&self, token: &str, metadata: FileMetadata) -> AppResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write().await;
        guard.insert(token.to_string(), metadata);
        drop(guard);
        Ok(())
    }

    async fn load(&self, token: &str) -> AppResult<FileMetadata> {
        let shard = self.get_shard_for_token(token);
        let guard = shard.read().await;
        let metadata = guard.get(token).cloned();
        drop(guard);
        metadata.ok_or_else(|| AppError::NotFound("file not found".to_string()))
    }

    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write().await;
        let metadata = guard
            .get_mut(token)
            .ok_or_else(|| AppError::NotFound("file not found".to_string()))?;

        let claim =
            if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit {
                DownloadClaim::LimitReached
            } else {
                metadata.download_count += 1;
                DownloadClaim::Allowed {
                    new_count: metadata.download_count,
                }
            };
        drop(guard);
        Ok(claim)
    }

    async fn remove(&self, token: &str) -> AppResult<()> {
        let shard = self.get_shard_for_token(token);
        let mut guard = shard.write().await;
        guard.remove(token);
        drop(guard);
        Ok(())
    }

    async fn list_expired(&self, limit: usize) -> AppResult<Vec<StoredFile>> {
        let mut expired = Vec::new();
        let now = Utc::now();

        for shard in &self.shards {
            let guard = shard.read().await;
            for (token, meta) in guard.iter() {
                if now >= meta.expires_at {
                    expired.push((token.clone(), meta.clone()));
                    if expired.len() >= limit {
                        return Ok(expired);
                    }
                }
            }
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
        let mut all_files = Vec::new();
        let now = Utc::now();

        for shard in &self.shards {
            let guard = shard.read().await;
            for (token, meta) in guard.iter() {
                if now < meta.expires_at {
                    all_files.push((token.clone(), meta.clone()));
                }
            }
        }

        let (paginated, total) = sort_and_paginate_files(all_files, offset, limit, sort_by, desc);

        Ok((paginated, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::sync::Arc;

    fn metadata(expires_at: chrono::DateTime<Utc>, download_limit: i32) -> FileMetadata {
        FileMetadata {
            original_name: "example.txt".to_string(),
            size: 7,
            extension: "txt".to_string(),
            uploaded_at: Utc::now(),
            expires_at,
            download_count: 0,
            download_limit,
        }
    }

    #[tokio::test]
    async fn save_preserves_expiration_and_expired_items_stay_paired() {
        let store = MemoryStore::new();
        let expires_at = Utc::now() - Duration::seconds(1);
        store
            .save("expired", metadata(expires_at, 0))
            .await
            .unwrap();

        assert_eq!(store.load("expired").await.unwrap().expires_at, expires_at);
        let expired = store.list_expired(10).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, "expired");
        assert_eq!(expired[0].1.expires_at, expires_at);
    }

    #[tokio::test]
    async fn concurrent_download_claims_never_exceed_the_limit() {
        let store = Arc::new(MemoryStore::new());
        store
            .save("limited", metadata(Utc::now() + Duration::hours(1), 10))
            .await
            .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let store = Arc::clone(&store);
            tasks.spawn(async move { store.claim_download("limited").await.unwrap() });
        }

        let mut allowed = 0;
        while let Some(result) = tasks.join_next().await {
            if matches!(result.unwrap(), DownloadClaim::Allowed { .. }) {
                allowed += 1;
            }
        }

        assert_eq!(allowed, 10);
        assert_eq!(store.load("limited").await.unwrap().download_count, 10);
    }
}
