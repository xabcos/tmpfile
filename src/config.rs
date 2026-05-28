use chrono::Duration;
use std::env;
use tracing::info;

pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";
pub const DEFAULT_UPLOAD_DIR: &str = "./uploads";
pub const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024; // 256MB
pub const DEFAULT_FILE_EXPIRATION: i64 = 36 * 3600; // 36 hours in seconds
pub const DEFAULT_MAX_DOWNLOAD_COUNT: i32 = 0; // 0 means unlimited

pub const MEMORY_SHARD_COUNT: usize = 64;
pub const CLEANUP_BATCH_SIZE: usize = 128;
pub const STORAGE_SUBDIR_DEPTH: usize = 2;
pub const STARTUP_CLEANUP_CONCURRENCY: usize = 32;
pub const MIN_CLEANUP_INTERVAL_SECS: i64 = 60;
pub const MAX_CLEANUP_INTERVAL_SECS: i64 = 24 * 3600;
pub const FILE_WRITE_BUFFER_SIZE: usize = 128 * 1024;

pub const TOKEN_CHARSET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
pub const TOKEN_LENGTH: usize = 8;
pub const TOKEN_TIME_FORMAT: &str = "%m%d%H%M";
pub const TOKEN_TIMESTAMP_LENGTH: usize = 8; // TOKEN_TIME_FORMAT output length
pub const TOKEN_TOTAL_LENGTH: usize = TOKEN_LENGTH + TOKEN_TIMESTAMP_LENGTH;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub upload_dir: String,
    pub max_file_size: u64,
    pub file_expiration: Duration,
    pub cleanup_interval: Duration,
    pub max_download_count: i32,
    pub image_max_download_count: i32,
    pub public_base_url: Option<String>,
    pub api_key: Option<String>,
    pub redis_addr: Option<String>,
    pub redis_db: i64,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let max_file_size = parse_env_u64("MAX_FILE_SIZE", DEFAULT_MAX_FILE_SIZE);
        let file_expiration = parse_env_duration("FILE_EXPIRATION", DEFAULT_FILE_EXPIRATION);
        let cleanup_interval = resolve_cleanup_interval(file_expiration);

        ServerConfig {
            listen_addr: env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string()),
            upload_dir: env::var("UPLOAD_DIR").unwrap_or_else(|_| DEFAULT_UPLOAD_DIR.to_string()),
            max_file_size,
            file_expiration,
            cleanup_interval,
            max_download_count: parse_env_int("MAX_DOWNLOAD_COUNT", DEFAULT_MAX_DOWNLOAD_COUNT),
            image_max_download_count: parse_env_int("IMAGE_MAX_DOWNLOAD_COUNT", 0),
            public_base_url: env::var("PUBLIC_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            api_key: env::var("API_KEY").ok().filter(|s| !s.is_empty()),
            redis_addr: env::var("REDIS_ADDR").ok().filter(|s| !s.is_empty()),
            redis_db: env::var("REDIS_DB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }
}

fn parse_duration_value(value: &str) -> Option<Duration> {
    value
        .parse::<i64>()
        .ok()
        .map(Duration::seconds)
        .or_else(|| {
            if let Some(stripped) = value.strip_suffix('h') {
                stripped.parse::<i64>().ok().map(Duration::hours)
            } else if let Some(stripped) = value.strip_suffix('d') {
                stripped.parse::<i64>().ok().map(Duration::days)
            } else if let Some(stripped) = value.strip_suffix('m') {
                stripped.parse::<i64>().ok().map(Duration::minutes)
            } else if let Some(stripped) = value.strip_suffix('s') {
                stripped.parse::<i64>().ok().map(Duration::seconds)
            } else {
                None
            }
        })
}

fn parse_env_duration(key: &str, default: i64) -> Duration {
    env::var(key)
        .ok()
        .and_then(|v| parse_duration_value(&v))
        .filter(|d| *d > Duration::zero())
        .unwrap_or_else(|| {
            info!("Using default {} seconds for {}", default, key);
            Duration::seconds(default)
        })
}

fn clamp_duration(value: Duration, min: Duration, max: Duration) -> Duration {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn resolve_cleanup_interval(file_expiration: Duration) -> Duration {
    let desired = env::var("CLEANUP_INTERVAL")
        .ok()
        .and_then(|v| parse_duration_value(&v))
        .unwrap_or(file_expiration / 2);

    let min = Duration::seconds(MIN_CLEANUP_INTERVAL_SECS);
    let max_candidate = Duration::seconds(MAX_CLEANUP_INTERVAL_SECS);

    if file_expiration < min {
        let upper = std::cmp::max(file_expiration, Duration::seconds(1));
        return clamp_duration(desired, Duration::seconds(1), upper);
    }

    let max = std::cmp::min(max_candidate, file_expiration);
    clamp_duration(desired, min, max)
}

fn parse_env_int(key: &str, default: i32) -> i32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            info!("Using default {} for {}", default, key);
            default
        })
}

fn parse_env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            info!("Using default {} for {}", default, key);
            default
        })
}
