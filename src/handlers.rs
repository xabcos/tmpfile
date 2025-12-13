use crate::config::TOKEN_TOTAL_LENGTH;
use crate::error::AppError;
use crate::models::{FileItem, ListFilesResponse, UploadResponse};
use crate::storage::{FileServer, TokenGenerator};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::IntoResponse,
};
use chrono::Utc;
use std::sync::Arc;
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

type AppResult<T> = Result<T, AppError>;

#[derive(serde::Deserialize)]
pub(crate) struct ListFilesQuery {
    page: Option<usize>,
    size: Option<usize>,
    sort_by: Option<String>,
    order: Option<String>,
}

fn require_api_key(headers: &HeaderMap, expected_api_key: Option<&str>) -> AppResult<()> {
    let Some(expected) = expected_api_key else {
        return Ok(());
    };

    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    let provided_key = auth_header.strip_prefix("Bearer ").unwrap_or("");
    if provided_key != expected {
        return Err(AppError::Unauthorized);
    }

    Ok(())
}

fn request_base_url(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.eq_ignore_ascii_case("https"))
        .map(|_| "https")
        .unwrap_or("http");

    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    format!("{}://{}", scheme, host)
}

pub async fn handle_upload(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<impl IntoResponse> {
    require_api_key(&headers, server.config.api_key.as_deref())?;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("Multipart error".to_string()))?
    {
        if field.name() == Some("file") {
            let filename = field
                .file_name()
                .ok_or(AppError::BadRequest("Missing filename".to_string()))?
                .to_string();

            if filename.is_empty() {
                continue;
            }

            let token = TokenGenerator::generate();
            // save_file now returns AppResult
            let metadata = server.save_file(&token, field, &filename).await?;

            let ttl = if server.config.redis_addr.is_some() {
                server.config.file_expiration + server.config.cleanup_interval
            } else {
                server.config.file_expiration
            };

            server
                .store
                .save(
                    &token,
                    metadata.clone(),
                    ttl.to_std().map_err(|e| {
                        AppError::InternalServerError(format!("Invalid TTL duration: {}", e))
                    })?,
                )
                .await
                .map_err(|e| {
                    error!("Failed to save metadata: {}", e);
                    AppError::InternalServerError("Failed to save metadata".to_string())
                })?;

            let base_url = request_base_url(&headers);

            let mut response = UploadResponse {
                direct_link: format!("{}/files/{}{}", base_url, token, metadata.extension),
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

    Err(AppError::BadRequest("No file field found".to_string()))
}

pub async fn handle_download(
    State(server): State<Arc<FileServer>>,
    Path(path): Path<String>,
) -> AppResult<impl IntoResponse> {
    let token = path
        .get(..TOKEN_TOTAL_LENGTH)
        .ok_or_else(|| AppError::BadRequest("Invalid token".to_string()))?;

    let metadata = server.store.load(token).await?;

    if Utc::now() > metadata.expires_at {
        return Err(AppError::Gone("File expired".to_string()));
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
        return Err(AppError::Gone("Download limit reached".to_string()));
    }

    // Open file for streaming
    let file = File::open(&metadata.storage_path)
        .await
        .map_err(|_| AppError::NotFound("File on disk not found".to_string()))?;

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
            Err(_) => {
                // Ignore errors in background task
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

pub async fn handle_list_files(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<ListFilesQuery>,
) -> AppResult<impl IntoResponse> {
    require_api_key(&headers, server.config.api_key.as_deref())?;

    // Parse query parameters
    let page = params.page.unwrap_or(1).max(1);
    let size = params.size.unwrap_or(100).clamp(1, 100);
    let sort_by = params.sort_by.as_deref().unwrap_or("uploaded_at");
    let desc = params
        .order
        .as_deref()
        .unwrap_or("desc")
        .eq_ignore_ascii_case("desc");

    // Calculate offset
    let offset = (page - 1) * size;

    // Get file list
    let (files, total) = server.store.list_files(offset, size, sort_by, desc).await?;

    // Build response with download links
    let base_url = request_base_url(&headers);

    let file_items: Vec<FileItem> = files
        .into_iter()
        .map(|(token, metadata)| FileItem {
            download_link: format!("{}/files/{}{}", base_url, token, metadata.extension),
            filename: metadata.original_name.clone(),
            size: metadata.size,
            uploaded_at: metadata.uploaded_at,
            expires_at: metadata.expires_at,
            download_count: metadata.download_count,
            token,
        })
        .collect();

    let total_pages = total.div_ceil(size).max(1);

    let response = ListFilesResponse {
        files: file_items,
        total,
        page,
        size,
        total_pages,
    };

    Ok(Json(response))
}
