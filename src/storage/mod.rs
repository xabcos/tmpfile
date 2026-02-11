mod file_server;
mod memory_store;
mod redis_store;
mod token;

use crate::error::AppError;
use crate::models::FileMetadata;
use std::time::Duration as StdDuration;

pub use file_server::FileServer;
pub use token::TokenGenerator;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadClaim {
    Allowed { new_count: i32 },
    LimitReached,
}

#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    async fn save(&self, token: &str, metadata: FileMetadata, ttl: StdDuration) -> AppResult<()>;
    async fn load(&self, token: &str) -> AppResult<FileMetadata>;
    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim>;
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

type AppResult<T> = Result<T, AppError>;

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
