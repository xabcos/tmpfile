mod file_server;
mod memory_store;
mod redis_store;
mod token;

use crate::config::MAX_FILE_EXTENSION_LENGTH;
use crate::error::AppError;
use crate::models::FileMetadata;

pub use file_server::FileServer;
pub use token::TokenGenerator;

pub type StoredFile = (String, FileMetadata);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSort {
    UploadedAt,
    Size,
    Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadClaim {
    Allowed { new_count: i32 },
    LimitReached,
}

#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    async fn save(&self, token: &str, metadata: FileMetadata) -> AppResult<()>;
    async fn load(&self, token: &str) -> AppResult<FileMetadata>;
    async fn claim_download(&self, token: &str) -> AppResult<DownloadClaim>;
    async fn remove(&self, token: &str) -> AppResult<()>;
    async fn list_expired(&self, limit: usize) -> AppResult<Vec<StoredFile>>;
    async fn list_files(
        &self,
        offset: usize,
        limit: usize,
        sort_by: FileSort,
        desc: bool,
    ) -> AppResult<(Vec<StoredFile>, usize)>;
}

type AppResult<T> = Result<T, AppError>;

fn is_valid_extension(extension: &str) -> bool {
    extension.is_empty()
        || (extension.len() <= MAX_FILE_EXTENSION_LENGTH
            && extension
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
}

fn sort_and_paginate_files(
    mut files: Vec<StoredFile>,
    offset: usize,
    limit: usize,
    sort_by: FileSort,
    desc: bool,
) -> (Vec<StoredFile>, usize) {
    files.sort_unstable_by(|left, right| {
        let primary = match sort_by {
            FileSort::UploadedAt => left.1.uploaded_at.cmp(&right.1.uploaded_at),
            FileSort::Size => left.1.size.cmp(&right.1.size),
            FileSort::Name => left.1.original_name.cmp(&right.1.original_name),
        };
        let primary = if desc { primary.reverse() } else { primary };

        primary.then_with(|| left.0.cmp(&right.0))
    });

    let total = files.len();
    let page = files.into_iter().skip(offset).take(limit).collect();
    (page, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    fn stored_file(token: &str, name: &str, size: i64, second: i64) -> StoredFile {
        let uploaded_at = Utc.timestamp_opt(second, 0).single().unwrap();
        (
            token.to_string(),
            FileMetadata {
                original_name: name.to_string(),
                size,
                extension: String::new(),
                is_video_ticket: false,
                uploaded_at,
                expires_at: uploaded_at + Duration::hours(1),
                download_count: 0,
                download_limit: 0,
            },
        )
    }

    #[test]
    fn sorting_is_deterministic_and_pagination_cannot_overflow() {
        let files = vec![
            stored_file("b", "same", 10, 1),
            stored_file("a", "same", 10, 1),
            stored_file("c", "other", 20, 2),
        ];

        let (page, total) = sort_and_paginate_files(files.clone(), 0, 2, FileSort::Name, false);
        assert_eq!(total, 3);
        assert_eq!(
            page.into_iter().map(|(token, _)| token).collect::<Vec<_>>(),
            ["c", "a"]
        );

        let (page, total) =
            sort_and_paginate_files(files, usize::MAX, usize::MAX, FileSort::Size, true);
        assert_eq!(total, 3);
        assert!(page.is_empty());
    }
}
