use crate::config::{FILE_READ_BUFFER_SIZE, MAX_FILENAME_BYTES, format_duration};
use crate::error::AppError;
use crate::models::{FileItem, ListFilesResponse, UploadResponse, VideoTicketResponse};
use crate::storage::{DownloadClaim, FileServer, FileSort, TokenGenerator};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::IntoResponse,
};
use chrono::Utc;
use std::{fmt::Write as _, sync::Arc};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

type AppResult<T> = Result<T, AppError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ListSortOrder {
    Asc,
    Desc,
}

#[derive(serde::Deserialize)]
pub(crate) struct ListFilesQuery {
    page: Option<usize>,
    size: Option<usize>,
    sort_by: Option<FileSort>,
    order: Option<ListSortOrder>,
}

impl ListFilesQuery {
    fn normalized(&self) -> (usize, usize, FileSort, bool) {
        let page = self.page.unwrap_or(1).max(1);
        let size = self.size.unwrap_or(100).clamp(1, 100);
        let sort_by = self.sort_by.unwrap_or(FileSort::UploadedAt);
        let desc = matches!(
            self.order.unwrap_or(ListSortOrder::Desc),
            ListSortOrder::Desc
        );

        (page, size, sort_by, desc)
    }
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

fn require_list_api_key(headers: &HeaderMap, expected_api_key: Option<&str>) -> AppResult<()> {
    let expected = expected_api_key
        .ok_or_else(|| AppError::NotFound("List endpoint is not enabled".to_string()))?;
    require_api_key(headers, Some(expected))
}

fn request_base_url(headers: &HeaderMap, public_base_url: Option<&str>) -> String {
    if let Some(base_url) = public_base_url {
        return base_url.to_string();
    }

    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"));
    let scheme = if is_https { "https" } else { "http" };

    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    format!("{scheme}://{host}")
}

fn download_url(base_url: &str, token: &str, extension: &str) -> String {
    if extension.is_empty() {
        format!("{base_url}/files/{token}")
    } else {
        format!("{base_url}/files/{token}.{extension}")
    }
}

fn video_token(path: &str) -> AppResult<&str> {
    let token = TokenGenerator::parse_path(path)
        .ok_or_else(|| AppError::BadRequest("Invalid video upload token".to_string()))?;
    if path.get(token.len()..) != Some(".mp4") {
        return Err(AppError::BadRequest(
            "Video upload URL must use the .mp4 extension".to_string(),
        ));
    }
    Ok(token)
}

fn content_type_for_extension(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "mp4" => "video/mp4",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn is_inline_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/png"
            | "image/jpeg"
            | "image/webp"
            | "image/gif"
            | "image/avif"
            | "image/x-icon"
            | "image/bmp"
            | "video/mp4"
    )
}

fn content_disposition(disposition: &str, filename: &str) -> HeaderValue {
    let fallback: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() && !matches!(character, '"' | '\\') {
                character
            } else {
                '_'
            }
        })
        .collect();

    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }

    HeaderValue::from_str(&format!(
        "{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

fn schedule_file_removal(
    server: Arc<FileServer>,
    token: String,
    metadata: crate::models::FileMetadata,
) {
    std::mem::drop(tokio::spawn(async move {
        if let Err(err) = server.remove_file(&token, &metadata).await {
            error!("Failed to remove {} after download: {}", token, err);
        }
    }));
}

pub async fn handle_upload(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<impl IntoResponse> {
    require_api_key(&headers, server.config.api_key.as_deref())?;

    while let Some(field) = multipart.next_field().await.map_err(AppError::from)? {
        if field.name() == Some("file") {
            let filename = field
                .file_name()
                .ok_or_else(|| AppError::BadRequest("Missing filename".to_string()))?
                .to_string();

            if filename.is_empty() {
                return Err(AppError::BadRequest("Filename cannot be empty".to_string()));
            }
            if filename.len() > MAX_FILENAME_BYTES {
                return Err(AppError::BadRequest(format!(
                    "Filename exceeds {MAX_FILENAME_BYTES} bytes"
                )));
            }

            let token = TokenGenerator::generate();
            let metadata = server.save_file(&token, field, &filename).await?;

            let base_url = request_base_url(&headers, server.config.public_base_url.as_deref());

            let mut response = UploadResponse {
                direct_link: download_url(&base_url, &token, &metadata.extension),
                filename: metadata.original_name.clone(),
                size: metadata.size,
                token: token.clone(),
                expires_at: metadata.expires_at,
                expires_in: format_duration(server.config.file_expiration),
                max_downloads: None,
                remaining_downloads: None,
            };

            if metadata.download_limit > 0 {
                response.max_downloads = Some(metadata.download_limit);
                response.remaining_downloads = Some(metadata.download_limit);
            }

            info!(
                "Uploaded: {} ({} bytes) -> {}",
                metadata.original_name, metadata.size, token
            );

            return Ok(Json(response));
        }
    }

    Err(AppError::BadRequest("No file field found".to_string()))
}

/// Creates a capability URL for an xAI ZDR video result. This endpoint is
/// deliberately unauthenticated: the high-entropy token is the capability,
/// and YAI's project key is not available on xAI's subsequent server-to-server
/// PUT request.
pub async fn handle_video_ticket(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    let token = server.create_video_ticket().await?;
    let base_url = request_base_url(&headers, server.config.public_base_url.as_deref());
    let response = VideoTicketResponse {
        // xAI may echo output.upload_url as video.url after completion, so the
        // one-time PUT capability also remains readable after upload.
        upload_url: download_url(&base_url, &token, "mp4"),
    };

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    info!("Created unauthenticated video upload ticket");
    Ok((StatusCode::CREATED, response_headers, Json(response)))
}

/// Receives the generated video bytes. xAI uses a raw PUT rather than the
/// multipart format accepted by `/upload`, so this path is streamed directly
/// to disk and can be used exactly as `output.upload_url`.
pub async fn handle_video_upload(
    State(server): State<Arc<FileServer>>,
    Path(path): Path<String>,
    body: Body,
) -> AppResult<impl IntoResponse> {
    let token = video_token(&path)?;
    let size = server.complete_video_upload(token, body).await?;

    info!("Accepted unauthenticated video upload ({size} stored bytes)");
    // Match the successful status returned by an S3 PutObject target, which is
    // the upload behavior xAI's official Grok Build flow is designed around.
    Ok(StatusCode::OK)
}

pub async fn handle_download(
    method: Method,
    State(server): State<Arc<FileServer>>,
    Path(path): Path<String>,
) -> AppResult<impl IntoResponse> {
    let token = TokenGenerator::parse_path(&path)
        .ok_or_else(|| AppError::BadRequest("Invalid token".to_string()))?;

    let metadata = server.store.load(token).await?;

    if Utc::now() >= metadata.expires_at {
        return Err(AppError::Gone("File expired".to_string()));
    }
    if metadata.is_video_ticket && metadata.size == 0 {
        return Err(AppError::NotFound("Video output is not ready".to_string()));
    }

    if metadata.download_limit > 0 && metadata.download_count >= metadata.download_limit {
        if method != Method::HEAD {
            schedule_file_removal(server, token.to_string(), metadata);
        }
        return Err(AppError::Gone("Download limit reached".to_string()));
    }

    let storage_path = server.get_file_path(token, &metadata.extension)?;
    let file = File::open(&storage_path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound("File on disk not found".to_string())
        } else {
            AppError::from(err)
        }
    })?;

    let content_type = content_type_for_extension(&metadata.extension);
    let disposition = if is_inline_content_type(content_type) {
        "inline"
    } else {
        "attachment"
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(disposition, &metadata.original_name),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.size.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    // Token URLs have expiration and download-count semantics. Explicitly
    // prevent browsers and reverse proxies from serving a cached response
    // without consulting the API.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );

    if method == Method::HEAD {
        return Ok((headers, Body::empty()));
    }

    let claim = server.store.claim_download(token).await?;

    if matches!(claim, DownloadClaim::LimitReached) {
        schedule_file_removal(server, token.to_string(), metadata);
        return Err(AppError::Gone("Download limit reached".to_string()));
    }

    let DownloadClaim::Allowed { new_count } = claim else {
        return Err(AppError::InternalServerError(
            "unexpected download claim result".to_string(),
        ));
    };

    info!(
        "Download: {} -> {} ({}/{})",
        metadata.original_name,
        token,
        new_count,
        if metadata.download_limit > 0 {
            metadata.download_limit.to_string()
        } else {
            "unlimited".to_string()
        }
    );

    if metadata.download_limit > 0 && new_count >= metadata.download_limit {
        schedule_file_removal(server.clone(), token.to_string(), metadata.clone());
    }

    let stream = ReaderStream::with_capacity(file, FILE_READ_BUFFER_SIZE);
    let body = Body::from_stream(stream);

    Ok((headers, body))
}

pub async fn handle_list_files(
    State(server): State<Arc<FileServer>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<ListFilesQuery>,
) -> AppResult<impl IntoResponse> {
    require_list_api_key(&headers, server.config.api_key.as_deref())?;

    let (page, size, sort_by, desc) = params.normalized();

    let offset = page
        .checked_sub(1)
        .and_then(|page_index| page_index.checked_mul(size))
        .ok_or_else(|| AppError::BadRequest("Page is too large".to_string()))?;

    let (files, total) = server.store.list_files(offset, size, sort_by, desc).await?;

    // Build response with download links
    let base_url = request_base_url(&headers, server.config.public_base_url.as_deref());

    let file_items: Vec<FileItem> = files
        .into_iter()
        .map(|(token, metadata)| FileItem {
            download_link: download_url(&base_url, &token, &metadata.extension),
            filename: metadata.original_name,
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
