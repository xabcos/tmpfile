use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};

fn serialize_datetime<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_rfc3339())
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub original_name: String,
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
    #[serde(serialize_with = "serialize_datetime")]
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
    #[serde(serialize_with = "serialize_datetime")]
    pub uploaded_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_datetime")]
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
