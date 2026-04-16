use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub execution_environment: String,
    pub database_url: String,
    pub postgres_max_connections: u32,
    pub postgres_min_connections: u32,
    pub postgres_acquire_timeout: Duration,
    pub deepgram_api_key: String,
    pub gemini_api_key: String,
    pub gemini_model: String,
    pub owner_chat_id: i64,
    pub port: u16,
    pub webhook_url: Url,
    pub yt_dlp_path: String,
    pub downloads_dir: PathBuf,
    pub audio_cache_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Missing required environment variable {0}")]
    Missing(&'static str),
    #[error("Invalid value for {name}: {value}")]
    Invalid { name: &'static str, value: String },
    #[error("Failed to create directory {path}: {source}")]
    Directory {
        path: String,
        source: std::io::Error,
    },
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let execution_environment =
            std::env::var("EXECUTION_ENVIRONMENT").unwrap_or_else(|_| "local".to_string());
        let database_url = required("DATABASE_URL")?;
        let postgres_max_connections = parse_env("POSTGRES_MAX_CONNECTIONS", 10u32)?;
        let postgres_min_connections = parse_env("POSTGRES_MIN_CONNECTIONS", 0u32)?;
        if postgres_min_connections > postgres_max_connections {
            return Err(ConfigError::Invalid {
                name: "POSTGRES_MIN_CONNECTIONS",
                value: postgres_min_connections.to_string(),
            });
        }
        let postgres_acquire_timeout_secs = parse_env("POSTGRES_ACQUIRE_TIMEOUT_SECS", 5u64)?;
        let deepgram_api_key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_default();
        let gemini_api_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
        let gemini_model = std::env::var("GEMINI_MODEL")
            .unwrap_or_else(|_| "gemini-3.1-flash-lite-preview".to_string());
        let owner_chat_id = parse_env("OWNER_CHAT_ID", 0i64)?;
        let port = parse_env("PORT", 8080u16)?;
        let webhook_url = required("WEBHOOK_URL")?
            .parse()
            .map_err(|_| ConfigError::Invalid {
                name: "WEBHOOK_URL",
                value: std::env::var("WEBHOOK_URL").unwrap_or_default(),
            })?;
        let yt_dlp_path = std::env::var("YT_DLP_PATH").unwrap_or_else(|_| "yt-dlp".to_string());
        let downloads_dir = PathBuf::from(
            std::env::var("DOWNLOADS_DIR").unwrap_or_else(|_| "/downloads".to_string()),
        );
        let audio_cache_dir = PathBuf::from(
            std::env::var("AUDIO_CACHE_DIR")
                .unwrap_or_else(|_| downloads_dir.join("audio_cache").to_string_lossy().into()),
        );

        ensure_dir(&downloads_dir)?;
        ensure_dir(&audio_cache_dir)?;

        Ok(Self {
            execution_environment,
            database_url,
            postgres_max_connections,
            postgres_min_connections,
            postgres_acquire_timeout: Duration::from_secs(postgres_acquire_timeout_secs),
            deepgram_api_key,
            gemini_api_key,
            gemini_model,
            owner_chat_id,
            port,
            webhook_url,
            yt_dlp_path,
            downloads_dir,
            audio_cache_dir,
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    std::env::var(name).map_err(|_| ConfigError::Missing(name))
}

fn parse_env<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|_| ConfigError::Invalid { name, value }),
        Err(_) => Ok(default),
    }
}

fn ensure_dir(path: &std::path::Path) -> Result<(), ConfigError> {
    std::fs::create_dir_all(path).map_err(|source| ConfigError::Directory {
        path: path.display().to_string(),
        source,
    })
}
