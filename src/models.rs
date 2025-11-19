use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub original_name: String,
    pub storage_path: String,
    pub size: i64,
    pub extension: String,
    pub uploaded_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub download_count: i32,
    pub download_limit: i32,
}

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub direct_link: String,
    pub filename: String,
    pub size: i64,
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub expires_in: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_downloads: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_downloads: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct FileItem {
    pub token: String,
    pub filename: String,
    pub size: i64,
    pub uploaded_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub download_count: i32,
    pub download_link: String,
}

#[derive(Debug, Serialize)]
pub struct ListFilesResponse {
    pub files: Vec<FileItem>,
    pub total: usize,
    pub page: usize,
    pub size: usize,
    pub total_pages: usize,
}

// Helper struct for Redis serialization
#[derive(Serialize, Deserialize)]
pub struct SerializableMetadata {
    pub original_name: String,
    pub storage_path: String,
    pub size: i64,
    pub extension: String,
    pub uploaded_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub download_count: i32,
    pub download_limit: i32,
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
