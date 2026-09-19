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
    pub telegram_token: String,
    pub deepgram_api_key: String,
    pub openrouter_api_key: String,
    pub openrouter_model: String,
    pub owner_chat_id: i64,
    pub port: u16,
    pub webhook_url: Url,
    pub downloader_socket: PathBuf,
    pub audio_cache_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Missing required environment variable {0}")]
    Missing(&'static str),
    #[error("Invalid value for {name}: {value}")]
    Invalid { name: &'static str, value: String },
    #[error("Failed to read secret file {name} at {path}: {source}")]
    SecretFile {
        name: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let execution_environment =
            std::env::var("EXECUTION_ENVIRONMENT").unwrap_or_else(|_| "local".to_string());
        let mut database_url = required("DATABASE_URL")?;
        if let Ok(path) = std::env::var("DATABASE_PASSWORD_FILE") {
            let password = read_secret_file("DATABASE_PASSWORD", &path)?;
            let mut url = Url::parse(&database_url).map_err(|_| ConfigError::Invalid {
                name: "DATABASE_URL",
                value: "invalid URL".to_string(),
            })?;
            url.set_password(Some(&password))
                .map_err(|_| ConfigError::Invalid {
                    name: "DATABASE_URL",
                    value: "URL cannot contain a password".to_string(),
                })?;
            database_url = url.to_string();
        }
        let postgres_max_connections = parse_env("POSTGRES_MAX_CONNECTIONS", 10u32)?;
        let postgres_min_connections = parse_env("POSTGRES_MIN_CONNECTIONS", 0u32)?;
        if postgres_min_connections > postgres_max_connections {
            return Err(ConfigError::Invalid {
                name: "POSTGRES_MIN_CONNECTIONS",
                value: postgres_min_connections.to_string(),
            });
        }
        let postgres_acquire_timeout_secs = parse_env("POSTGRES_ACQUIRE_TIMEOUT_SECS", 5u64)?;
        let telegram_token = secret_env("TELOXIDE_TOKEN")?;
        let deepgram_api_key = secret_env("DEEPGRAM_API_KEY")?;
        let openrouter_api_key = secret_env("OPENROUTER_API_KEY")?;
        let openrouter_model =
            std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-5.6-luna".to_string());
        let owner_chat_id = parse_secret_env("OWNER_CHAT_ID", 0i64)?;
        let port = parse_env("PORT", 8080u16)?;
        let webhook_url = required("WEBHOOK_URL")?
            .parse()
            .map_err(|_| ConfigError::Invalid {
                name: "WEBHOOK_URL",
                value: std::env::var("WEBHOOK_URL").unwrap_or_default(),
            })?;
        let downloader_socket = PathBuf::from(
            std::env::var("DOWNLOADER_SOCKET")
                .unwrap_or_else(|_| "/downloader/downloader.sock".to_string()),
        );
        let audio_cache_dir = PathBuf::from("/downloads/audio_cache");

        Ok(Self {
            execution_environment,
            database_url,
            postgres_max_connections,
            postgres_min_connections,
            postgres_acquire_timeout: Duration::from_secs(postgres_acquire_timeout_secs),
            telegram_token,
            deepgram_api_key,
            openrouter_api_key,
            openrouter_model,
            owner_chat_id,
            port,
            webhook_url,
            downloader_socket,
            audio_cache_dir,
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    std::env::var(name).map_err(|_| ConfigError::Missing(name))
}

fn secret_env(name: &'static str) -> Result<String, ConfigError> {
    let file_name = format!("{name}_FILE");
    match std::env::var(&file_name) {
        Ok(path) => read_secret_file(name, &path),
        Err(_) => Ok(std::env::var(name).unwrap_or_default()),
    }
}

fn read_secret_file(name: &'static str, path: &str) -> Result<String, ConfigError> {
    std::fs::read_to_string(path)
        .map(|value| value.trim_end().to_string())
        .map_err(|source| ConfigError::SecretFile {
            name,
            path: path.to_string(),
            source,
        })
}

fn parse_secret_env<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    let value = secret_env(name)?;
    if value.is_empty() {
        Ok(default)
    } else {
        value
            .parse::<T>()
            .map_err(|_| ConfigError::Invalid { name, value })
    }
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
