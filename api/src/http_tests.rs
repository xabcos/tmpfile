use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode, header},
};
use chrono::Duration;
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;
use tower::ServiceExt;

const BOUNDARY: &str = "tmpfile-test-boundary";

fn upload_contract() -> Value {
    serde_json::from_str(include_str!("../contracts/upload.json"))
        .expect("upload contract fixture must be valid JSON")
}

fn test_config(upload_dir: &Path, max_file_size: u64, api_key: Option<&str>) -> ServerConfig {
    ServerConfig {
        listen_addr: "127.0.0.1:0".to_string(),
        upload_dir: upload_dir.to_string_lossy().into_owned(),
        max_file_size,
        file_expiration: Duration::minutes(90),
        cleanup_interval: Duration::minutes(5),
        max_download_count: 0,
        image_max_download_count: 0,
        public_base_url: Some("https://files.example.test".to_string()),
        api_key: api_key.map(str::to_string),
        redis_addr: None,
        redis_db: 0,
    }
}

async fn test_app(max_file_size: u64, api_key: Option<&str>) -> (Router, Arc<FileServer>, TempDir) {
    test_app_with_download_limit(max_file_size, api_key, 0).await
}

async fn test_app_with_download_limit(
    max_file_size: u64,
    api_key: Option<&str>,
    max_download_count: i32,
) -> (Router, Arc<FileServer>, TempDir) {
    let upload_dir = tempfile::tempdir().unwrap();
    let mut config = test_config(upload_dir.path(), max_file_size, api_key);
    config.max_download_count = max_download_count;
    let server = Arc::new(FileServer::new(config).await.unwrap());
    server.initialize_storage().await.unwrap();

    (build_app(Arc::clone(&server)), server, upload_dir)
}

fn multipart(filename: &str, contents: &[u8], complete: bool) -> Vec<u8> {
    let contract = upload_contract();
    let field = contract["multipart_field"].as_str().unwrap();
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(contents);
    if complete {
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    }
    body
}

fn upload_request(filename: &str, contents: &[u8], api_key: Option<&str>) -> Request<Body> {
    let contract = upload_contract();
    let mut builder = Request::builder()
        .method(contract["method"].as_str().unwrap())
        .uri(contract["path"].as_str().unwrap())
        .header(header::HOST, "ignored.example.test")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
    if let Some(api_key) = api_key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {api_key}"));
    }

    builder
        .body(Body::from(multipart(filename, contents, true)))
        .unwrap()
}

async fn response_json(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn count_files(path: &Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .map(|path| if path.is_dir() { count_files(&path) } else { 1 })
        .sum()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn upload_download_and_list_cover_the_http_contract_and_boundaries() {
    let (app, _server, upload_dir) = test_app(5, Some("secret")).await;

    let response = app
        .clone()
        .oneshot(upload_request("picture.JPEG", b"hello", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The file content may be exactly MAX_FILE_SIZE; multipart framing gets a
    // separate bounded allowance.
    let response = app
        .clone()
        .oneshot(upload_request("report name.TXT", b"hello", Some("secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let upload = response_json(response).await;
    for field in upload_contract()["required_response_fields"]
        .as_array()
        .unwrap()
    {
        let field = field.as_str().unwrap();
        assert!(upload.get(field).is_some(), "missing upload field {field}");
    }
    assert_eq!(upload["size"], 5);
    assert_eq!(upload["expires_in"], "90m");
    assert_eq!(upload["token"].as_str().unwrap().len(), 25);
    assert!(
        upload["direct_link"]
            .as_str()
            .unwrap()
            .starts_with("https://files.example.test/files/")
    );
    assert_eq!(
        Path::new(upload["direct_link"].as_str().unwrap())
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("txt")
    );
    assert_eq!(count_files(upload_dir.path()), 1);

    let download_path = upload["direct_link"]
        .as_str()
        .unwrap()
        .strip_prefix("https://files.example.test")
        .unwrap();
    let response = app
        .clone()
        .oneshot(Request::get(download_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert!(response.headers().get(header::ACCEPT_RANGES).is_none());
    let disposition = response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap();
    assert!(disposition.contains("filename*=UTF-8''report%20name.TXT"));
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"hello"
    );

    let response = app
        .clone()
        .oneshot(
            Request::get("/list?sort_by=name&order=asc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(
            Request::get("/list?sort_by=name&order=asc")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listing = response_json(response).await;
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["files"][0]["filename"], "report name.TXT");

    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/list?page={}&size=100", usize::MAX))
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = app
        .clone()
        .oneshot(upload_request("too-big.txt", b"123456", Some("secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(count_files(upload_dir.path()), 1);

    let response = app
        .clone()
        .oneshot(
            Request::get("/files/not-a-valid-token.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn head_does_not_consume_the_download_limit() {
    let (app, server, upload_dir) = test_app_with_download_limit(1024, None, 1).await;

    let response = app
        .clone()
        .oneshot(upload_request("hello.txt", b"hello", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let upload = response_json(response).await;
    let token = upload["token"].as_str().unwrap();
    let download_path = upload["direct_link"]
        .as_str()
        .unwrap()
        .strip_prefix("https://files.example.test")
        .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(download_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );

    let metadata = server.store.load(token).await.unwrap();
    assert_eq!(metadata.download_count, 0);
    assert_eq!(count_files(upload_dir.path()), 1);

    let response = app
        .oneshot(Request::get(download_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"hello"
    );
}

#[tokio::test]
async fn open_video_ticket_accepts_one_raw_put_and_keeps_regular_uploads_protected() {
    let (app, _server, upload_dir) = test_app(5, Some("secret")).await;

    let response = app
        .clone()
        .oneshot(
            Request::post("/video-output/tickets")
                .header(header::HOST, "ignored.example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let ticket = response_json(response).await;
    assert!(
        ticket["upload_url"]
            .as_str()
            .unwrap()
            .starts_with("https://files.example.test/files/")
    );
    assert_eq!(ticket.as_object().unwrap().len(), 1);
    assert_eq!(count_files(upload_dir.path()), 0);

    let upload_path = ticket["upload_url"]
        .as_str()
        .unwrap()
        .strip_prefix("https://files.example.test")
        .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::put(upload_path)
                .header(header::CONTENT_TYPE, "video/mp4")
                .body(Body::from(b"video".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(count_files(upload_dir.path()), 1);

    let download_path = upload_path;
    let response = app
        .clone()
        .oneshot(Request::get(download_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert!(
        response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("inline;")
    );
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"video"
    );

    let response = app
        .clone()
        .oneshot(
            Request::put(upload_path)
                .body(Body::from(b"again".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(Request::get(upload_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"video",
        "an idempotent retry must not replace the completed video"
    );

    let response = app
        .clone()
        .oneshot(upload_request("ordinary.txt", b"hello", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A regular zero-byte MP4 is not a video ticket and must not become
    // publicly writable merely because its size and extension look similar.
    let response = app
        .clone()
        .oneshot(upload_request("empty.mp4", b"", Some("secret")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ordinary = response_json(response).await;
    let ordinary_path = ordinary["direct_link"]
        .as_str()
        .unwrap()
        .strip_prefix("https://files.example.test")
        .unwrap();
    let response = app
        .oneshot(
            Request::put(ordinary_path)
                .body(Body::from(b"overwrite".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn video_put_enforces_size_and_rejects_empty_or_invalid_targets() {
    let (app, _server, upload_dir) = test_app(5, Some("secret")).await;
    let response = app
        .clone()
        .oneshot(
            Request::post("/video-output/tickets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ticket = response_json(response).await;
    let upload_path = ticket["upload_url"]
        .as_str()
        .unwrap()
        .strip_prefix("https://files.example.test")
        .unwrap();

    let response = app
        .clone()
        .oneshot(Request::put(upload_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(count_files(upload_dir.path()), 0);

    let response = app
        .clone()
        .oneshot(
            Request::put(upload_path)
                .body(Body::from(b"123456".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(count_files(upload_dir.path()), 0);

    let response = app
        .clone()
        .oneshot(
            Request::put(upload_path)
                .body(Body::from(b"video".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(count_files(upload_dir.path()), 1);

    let response = app
        .oneshot(
            Request::put("/files/not-a-valid-token.mp4")
                .body(Body::from(b"video".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn incomplete_upload_is_removed_and_listing_is_disabled_without_an_api_key() {
    let (app, _server, upload_dir) = test_app(1024, None).await;

    let request = Request::post("/upload")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart("partial.txt", b"partial", false)))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(!response.status().is_success());
    assert_eq!(count_files(upload_dir.path()), 0);

    let response = app
        .oneshot(Request::get("/list").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn options_is_intentionally_left_to_the_reverse_proxy() {
    let (app, _server, _upload_dir) = test_app(1024, None).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/upload")
                .header(header::ORIGIN, "https://pages.example.test")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}
