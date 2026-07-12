use chrono::{Duration, Utc};
use std::{env, fmt};

pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8080";
pub const DEFAULT_UPLOAD_DIR: &str = "./uploads";
pub const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB
pub const DEFAULT_FILE_EXPIRATION: i64 = 36 * 3600; // 36 hours in seconds
pub const DEFAULT_MAX_DOWNLOAD_COUNT: i32 = 0; // 0 means unlimited

pub const MEMORY_SHARD_COUNT: usize = 64;
pub const CLEANUP_BATCH_SIZE: usize = 128;
pub const STORAGE_SUBDIR_DEPTH: usize = 2;
pub const STARTUP_CLEANUP_CONCURRENCY: usize = 32;
pub const MIN_CLEANUP_INTERVAL_SECS: i64 = 60;
pub const MAX_CLEANUP_INTERVAL_SECS: i64 = 24 * 3600;
pub const FILE_WRITE_BUFFER_SIZE: usize = 128 * 1024;
pub const FILE_READ_BUFFER_SIZE: usize = 128 * 1024;
pub const MULTIPART_OVERHEAD_BYTES: usize = 64 * 1024;
pub const MAX_FILENAME_BYTES: usize = 255;
pub const MAX_FILE_EXTENSION_LENGTH: usize = 16;

pub const TOKEN_CHARSET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
// 25 base-36 characters provide just over 129 bits of random token space.
pub const TOKEN_LENGTH: usize = 25;

#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    fn invalid(key: &str, value: &str, expected: &str) -> Self {
        Self(format!(
            "invalid {key} value {value:?}: expected {expected}"
        ))
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone)]
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
    pub fn from_env() -> Result<Self, ConfigError> {
        let max_file_size = parse_env_u64("MAX_FILE_SIZE", DEFAULT_MAX_FILE_SIZE)?;
        let file_expiration = parse_env_duration("FILE_EXPIRATION", DEFAULT_FILE_EXPIRATION)?;
        validate_file_expiration(file_expiration)?;
        let cleanup_interval = resolve_cleanup_interval(file_expiration)?;

        Ok(Self {
            listen_addr: optional_env("LISTEN_ADDR")?
                .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_string()),
            upload_dir: optional_env("UPLOAD_DIR")?
                .unwrap_or_else(|| DEFAULT_UPLOAD_DIR.to_string()),
            max_file_size,
            file_expiration,
            cleanup_interval,
            max_download_count: parse_env_nonnegative_i32(
                "MAX_DOWNLOAD_COUNT",
                DEFAULT_MAX_DOWNLOAD_COUNT,
            )?,
            image_max_download_count: parse_env_nonnegative_i32("IMAGE_MAX_DOWNLOAD_COUNT", 0)?,
            public_base_url: optional_env("PUBLIC_BASE_URL")?
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty()),
            api_key: optional_env("API_KEY")?.filter(|value| !value.is_empty()),
            redis_addr: optional_env("REDIS_ADDR")?.filter(|value| !value.is_empty()),
            redis_db: parse_env_nonnegative_i64("REDIS_DB", 0)?,
        })
    }
}

fn validate_file_expiration(file_expiration: Duration) -> Result<(), ConfigError> {
    Utc::now()
        .checked_add_signed(file_expiration)
        .map(|_| ())
        .ok_or_else(|| {
            ConfigError(
                "FILE_EXPIRATION exceeds the supported calendar range from the current time"
                    .to_string(),
            )
        })
}

fn optional_env(key: &str) -> Result<Option<String>, ConfigError> {
    match env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError(format!("{key} contains non-Unicode data")))
        }
    }
}

fn parse_duration_value(value: &str) -> Option<Duration> {
    let (number, unit) = value
        .strip_suffix('d')
        .map(|number| (number, 'd'))
        .or_else(|| value.strip_suffix('h').map(|number| (number, 'h')))
        .or_else(|| value.strip_suffix('m').map(|number| (number, 'm')))
        .or_else(|| value.strip_suffix('s').map(|number| (number, 's')))
        .unwrap_or((value, 's'));
    let amount = number.parse::<i64>().ok()?;

    match unit {
        'd' => Duration::try_days(amount),
        'h' => Duration::try_hours(amount),
        'm' => Duration::try_minutes(amount),
        _ => Duration::try_seconds(amount),
    }
}

fn parse_env_duration(key: &str, default_seconds: i64) -> Result<Duration, ConfigError> {
    let Some(value) = optional_env(key)? else {
        return Ok(Duration::seconds(default_seconds));
    };

    parse_duration_value(&value)
        .filter(|duration| *duration > Duration::zero())
        .ok_or_else(|| {
            ConfigError::invalid(key, &value, "a positive duration such as 30m, 2h, or 1d")
        })
}

fn clamp_duration(value: Duration, min: Duration, max: Duration) -> Duration {
    value.clamp(min, max)
}

fn resolve_cleanup_interval(file_expiration: Duration) -> Result<Duration, ConfigError> {
    let desired = match optional_env("CLEANUP_INTERVAL")? {
        Some(value) => parse_duration_value(&value)
            .filter(|duration| *duration > Duration::zero())
            .ok_or_else(|| {
                ConfigError::invalid(
                    "CLEANUP_INTERVAL",
                    &value,
                    "a positive duration such as 30m, 2h, or 1d",
                )
            })?,
        None => file_expiration / 2,
    };

    let min = Duration::seconds(MIN_CLEANUP_INTERVAL_SECS);
    if file_expiration < min {
        let upper = std::cmp::max(file_expiration, Duration::seconds(1));
        return Ok(clamp_duration(desired, Duration::seconds(1), upper));
    }

    let max = std::cmp::min(
        Duration::seconds(MAX_CLEANUP_INTERVAL_SECS),
        file_expiration,
    );
    Ok(clamp_duration(desired, min, max))
}

fn parse_env_nonnegative_i32(key: &str, default: i32) -> Result<i32, ConfigError> {
    let Some(value) = optional_env(key)? else {
        return Ok(default);
    };

    value
        .parse::<i32>()
        .ok()
        .filter(|parsed| *parsed >= 0)
        .ok_or_else(|| ConfigError::invalid(key, &value, "a non-negative 32-bit integer"))
}

fn parse_env_nonnegative_i64(key: &str, default: i64) -> Result<i64, ConfigError> {
    let Some(value) = optional_env(key)? else {
        return Ok(default);
    };

    value
        .parse::<i64>()
        .ok()
        .filter(|parsed| *parsed >= 0)
        .ok_or_else(|| ConfigError::invalid(key, &value, "a non-negative 64-bit integer"))
}

fn parse_env_u64(key: &str, default: u64) -> Result<u64, ConfigError> {
    let Some(value) = optional_env(key)? else {
        return Ok(default);
    };

    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| ConfigError::invalid(key, &value, "a positive integer"))
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.num_seconds();
    if seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_supports_units_without_panicking_on_extremes() {
        assert_eq!(parse_duration_value("90m"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration_value("2h"), Some(Duration::hours(2)));
        assert_eq!(parse_duration_value("3d"), Some(Duration::days(3)));
        assert_eq!(parse_duration_value("45"), Some(Duration::seconds(45)));
        assert_eq!(parse_duration_value("9223372036854775807d"), None);
        assert_eq!(parse_duration_value("invalid"), None);
        let large_but_parseable = parse_duration_value("100000000d").unwrap();
        assert!(validate_file_expiration(large_but_parseable).is_err());
    }

    #[test]
    fn duration_formatter_does_not_lose_precision() {
        assert_eq!(format_duration(Duration::hours(36)), "36h");
        assert_eq!(format_duration(Duration::minutes(90)), "90m");
        assert_eq!(format_duration(Duration::seconds(45)), "45s");
    }

    #[test]
    fn cleanup_interval_is_clamped_to_safe_bounds() {
        assert_eq!(
            clamp_duration(
                Duration::seconds(1),
                Duration::minutes(1),
                Duration::hours(1)
            ),
            Duration::minutes(1)
        );
        assert_eq!(
            clamp_duration(Duration::hours(2), Duration::minutes(1), Duration::hours(1)),
            Duration::hours(1)
        );
    }
}
