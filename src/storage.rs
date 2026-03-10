use async_trait::async_trait;
use sqlx::PgPool;

use crate::downloader::MediaType;

#[derive(Debug, Clone)]
pub struct CachedMedia {
    pub caption: String,
    pub files: Vec<CachedFile>,
}

#[derive(Debug, Clone)]
pub struct CachedFile {
    pub telegram_file_id: String,
    pub media_type: MediaType,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Storage: Send + Sync {
    async fn get_cached_media(&self, source_url: &str) -> Option<CachedMedia>;
    async fn store_cached_media(
        &self,
        source_url: &str,
        caption: &str,
        files: &[(String, MediaType)],
    );
    async fn log_request(
        &self,
        chat_id: i64,
        source_url: &str,
        status: &str,
        processing_time_ms: i64,
    );
}

pub struct PostgresStorage {
    pool: PgPool,
}

impl PostgresStorage {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(pool).await
    }

    pub async fn cleanup_expired(pool: &PgPool, ttl_days: i64) {
        let result = sqlx::query(
            "DELETE FROM media_cache WHERE last_used_at < NOW() - make_interval(days => $1)",
        )
        .bind(ttl_days)
        .execute(pool)
        .await;

        match result {
            Ok(r) => log::info!("Cache cleanup: removed {} expired entries", r.rows_affected()),
            Err(e) => log::error!("Cache cleanup failed: {}", e),
        }
    }
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn get_cached_media(&self, source_url: &str) -> Option<CachedMedia> {
        let cache_row: Option<(i32, String)> =
            sqlx::query_as("SELECT id, caption FROM media_cache WHERE source_url = $1")
                .bind(source_url)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    log::error!("Cache lookup failed: {}", e);
                    e
                })
                .ok()?;

        let (cache_id, caption) = cache_row?;

        // Update last_used_at
        let _ = sqlx::query("UPDATE media_cache SET last_used_at = NOW() WHERE id = $1")
            .bind(cache_id)
            .execute(&self.pool)
            .await;

        let file_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT telegram_file_id, media_type FROM cached_files WHERE cache_id = $1 ORDER BY position",
        )
        .bind(cache_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            log::error!("Cache files lookup failed: {}", e);
            e
        })
        .ok()?;

        if file_rows.is_empty() {
            return None;
        }

        let files: Vec<CachedFile> = file_rows
            .into_iter()
            .filter_map(|(file_id, media_type_str)| {
                let media_type = media_type_str.parse::<MediaType>().ok()?;
                Some(CachedFile {
                    telegram_file_id: file_id,
                    media_type,
                })
            })
            .collect();

        if files.is_empty() {
            return None;
        }

        Some(CachedMedia { caption, files })
    }

    async fn store_cached_media(
        &self,
        source_url: &str,
        caption: &str,
        files: &[(String, MediaType)],
    ) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                log::error!("Failed to begin transaction for {}: {}", source_url, e);
                return;
            }
        };

        let result: Result<(i32,), _> = sqlx::query_as(
            "INSERT INTO media_cache (source_url, caption) VALUES ($1, $2) \
             ON CONFLICT (source_url) DO UPDATE SET caption = $2, last_used_at = NOW() \
             RETURNING id",
        )
        .bind(source_url)
        .bind(caption)
        .fetch_one(&mut *tx)
        .await;

        let cache_id = match result {
            Ok((id,)) => id,
            Err(e) => {
                log::error!("Failed to store cache entry for {}: {}", source_url, e);
                return;
            }
        };

        // Delete old files for this cache entry (in case of ON CONFLICT update)
        if let Err(e) = sqlx::query("DELETE FROM cached_files WHERE cache_id = $1")
            .bind(cache_id)
            .execute(&mut *tx)
            .await
        {
            log::error!("Failed to delete old cached files for {}: {}", source_url, e);
            return;
        }

        for (position, (file_id, media_type)) in files.iter().enumerate() {
            if let Err(e) = sqlx::query(
                "INSERT INTO cached_files (cache_id, telegram_file_id, media_type, position) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(cache_id)
            .bind(file_id)
            .bind(media_type.to_string())
            .bind(position as i32)
            .execute(&mut *tx)
            .await
            {
                log::error!("Failed to store cached file: {}", e);
                return;
            }
        }

        if let Err(e) = tx.commit().await {
            log::error!(
                "Failed to commit cache transaction for {}: {}",
                source_url,
                e
            );
            return;
        }

        log::info!("Cached {} file(s) for {}", files.len(), source_url);
    }

    async fn log_request(
        &self,
        chat_id: i64,
        source_url: &str,
        status: &str,
        processing_time_ms: i64,
    ) {
        if let Err(e) = sqlx::query(
            "INSERT INTO requests (chat_id, source_url, status, processing_time_ms) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(chat_id)
        .bind(source_url)
        .bind(status)
        .bind(processing_time_ms)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to log request: {}", e);
        }
    }
}
