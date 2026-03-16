use async_trait::async_trait;
use sqlx::PgPool;

use crate::downloader::MediaType;
use crate::handler::CallbackContext;
use crate::subscription::{SubscriptionInfo, SubscriptionTier};

/// A payment record returned for self-service refund eligibility checks and owner tooling.
#[derive(Debug, Clone)]
pub struct PaymentRecord {
    pub telegram_charge_id: String,
    pub product: String,
    pub amount: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct CachedMedia {
    pub caption: String,
    pub files: Vec<CachedFile>,
    /// Path to the extracted audio file on disk, if it was extracted and still exists.
    pub audio_cache_path: Option<String>,
    /// Duration of the video in seconds, for AI quota accounting.
    pub media_duration_secs: Option<i32>,
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
        audio_cache_path: Option<String>,
        media_duration_secs: Option<i32>,
    );
    async fn log_request(
        &self,
        chat_id: i64,
        source_url: &str,
        status: &str,
        processing_time_ms: i64,
    );

    // Subscription management
    async fn get_subscription(&self, user_id: i64) -> SubscriptionInfo;
    async fn upsert_subscription(
        &self,
        user_id: i64,
        tier: SubscriptionTier,
        duration_days: i64,
    );

    // Payment recording
    async fn record_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
        provider_charge_id: &str,
        product: &str,
        amount: i32,
    );

    // AI Seconds tracking
    async fn consume_ai_seconds(&self, user_id: i64, seconds: i32);
    async fn add_topup_seconds(&self, user_id: i64, seconds: i32);
    async fn record_premium_usage(
        &self,
        user_id: i64,
        feature: &str,
        source_url: &str,
        duration_secs: i32,
        units: f64,
        cost_usd: f64,
    );

    // Callback context
    async fn store_callback_context(&self, ctx: &CallbackContext) -> i32;
    async fn get_callback_context(&self, context_id: i32) -> Option<CallbackContext>;
    async fn cache_transcript(&self, context_id: i32, transcript: &str, language: Option<&str>);

    // Subscription downgrade (for refunds)
    async fn revoke_subscription(&self, user_id: i64);
    /// Reduce top-up balance by `seconds` (clamped to 0). Used when a top-up purchase is refunded.
    async fn revoke_topup(&self, user_id: i64, seconds: i32);
    /// Returns the most recent payment for a user, if any.
    async fn get_latest_payment(&self, user_id: i64) -> Option<PaymentRecord>;
    /// Returns the most recent `limit` payments for a user (for owner tooling).
    async fn get_recent_payments(&self, user_id: i64, limit: i64) -> Vec<PaymentRecord>;
    /// Returns true if the user has any premium_usage rows recorded after `since`.
    async fn has_ai_usage_since(&self, user_id: i64, since: chrono::DateTime<chrono::Utc>) -> bool;

    // Cleanup
    async fn cleanup_expired_callback_contexts(&self);
    /// Zero out top-up balances whose last_topup_at exceeds TOPUP_EXPIRY_DAYS.
    async fn expire_stale_topups(&self);
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
        // Collect audio file paths to delete before removing DB rows
        let expired_audio: Vec<(Option<String>,)> = sqlx::query_as(
            "SELECT audio_cache_path FROM media_cache \
             WHERE last_used_at < NOW() - make_interval(days => $1::int)",
        )
        .bind(ttl_days)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let result = sqlx::query(
            "DELETE FROM media_cache WHERE last_used_at < NOW() - make_interval(days => $1::int)",
        )
        .bind(ttl_days)
        .execute(pool)
        .await;

        match result {
            Ok(r) => {
                log::info!("Cache cleanup: removed {} expired entries", r.rows_affected());
                for path in expired_audio.into_iter().filter_map(|(p,)| p) {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            log::warn!("Failed to delete expired audio file {}: {}", path, e);
                        }
                    }
                }
            }
            Err(e) => log::error!("Cache cleanup failed: {}", e),
        }
    }
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn get_cached_media(&self, source_url: &str) -> Option<CachedMedia> {
        let cache_row: Option<(i32, String, Option<String>, Option<i32>)> =
            sqlx::query_as(
                "SELECT id, caption, audio_cache_path, media_duration_secs \
                 FROM media_cache WHERE source_url = $1",
            )
            .bind(source_url)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                log::error!("Cache lookup failed: {}", e);
                e
            })
            .ok()?;

        let (cache_id, caption, audio_cache_path, media_duration_secs) = cache_row?;

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

        Some(CachedMedia { caption, files, audio_cache_path, media_duration_secs })
    }

    async fn store_cached_media(
        &self,
        source_url: &str,
        caption: &str,
        files: &[(String, MediaType)],
        audio_cache_path: Option<String>,
        media_duration_secs: Option<i32>,
    ) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                log::error!("Failed to begin transaction for {}: {}", source_url, e);
                return;
            }
        };

        let result: Result<(i32,), _> = sqlx::query_as(
            "INSERT INTO media_cache (source_url, caption, audio_cache_path, media_duration_secs) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (source_url) DO UPDATE \
             SET caption = $2, audio_cache_path = $3, media_duration_secs = $4, last_used_at = NOW() \
             RETURNING id",
        )
        .bind(source_url)
        .bind(caption)
        .bind(audio_cache_path)
        .bind(media_duration_secs)
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

    async fn get_subscription(&self, user_id: i64) -> SubscriptionInfo {
        let row: Option<(String, i32, i32, i32, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as(
                "SELECT tier, ai_seconds_used, ai_seconds_limit, topup_seconds_available, \
                 last_topup_at, expires_at FROM subscriptions WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                log::error!("Failed to get subscription for {}: {}", user_id, e);
                e
            })
            .ok()
            .flatten();

        match row {
            Some((tier_str, used, limit, topup, last_topup_at, expires_at)) => {
                let tier = tier_str.parse().unwrap_or(SubscriptionTier::Free);
                SubscriptionInfo {
                    tier,
                    ai_seconds_used: used,
                    ai_seconds_limit: limit,
                    topup_seconds_available: topup,
                    last_topup_at,
                    expires_at,
                }
            }
            None => SubscriptionInfo::free_default(),
        }
    }

    async fn upsert_subscription(
        &self,
        user_id: i64,
        tier: SubscriptionTier,
        duration_days: i64,
    ) {
        let limit = tier.ai_seconds_limit();
        let tier_str = tier.to_string();
        if let Err(e) = sqlx::query(
            "INSERT INTO subscriptions (user_id, tier, ai_seconds_used, ai_seconds_limit, expires_at, updated_at) \
             VALUES ($1, $2, 0, $3, NOW() + make_interval(days => $4::int), NOW()) \
             ON CONFLICT (user_id) DO UPDATE SET \
               tier = $2, ai_seconds_used = 0, ai_seconds_limit = $3, \
               expires_at = NOW() + make_interval(days => $4::int), updated_at = NOW()",
        )
        .bind(user_id)
        .bind(&tier_str)
        .bind(limit)
        .bind(duration_days)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to upsert subscription for {}: {}", user_id, e);
        }
    }

    async fn record_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
        provider_charge_id: &str,
        product: &str,
        amount: i32,
    ) {
        if let Err(e) = sqlx::query(
            "INSERT INTO payments (user_id, telegram_payment_charge_id, provider_payment_charge_id, product, amount) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (telegram_payment_charge_id) DO NOTHING",
        )
        .bind(user_id)
        .bind(telegram_charge_id)
        .bind(provider_charge_id)
        .bind(product)
        .bind(amount)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to record payment for {}: {}", user_id, e);
        }
    }

    async fn consume_ai_seconds(&self, user_id: i64, seconds: i32) {
        if let Err(e) = sqlx::query(
            "UPDATE subscriptions SET \
               ai_seconds_used = LEAST(ai_seconds_used + $2, ai_seconds_limit), \
               topup_seconds_available = GREATEST( \
                   topup_seconds_available - GREATEST($2 - (ai_seconds_limit - ai_seconds_used), 0), \
                   0 \
               ), \
               updated_at = NOW() \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(seconds)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to consume ai_seconds for {}: {}", user_id, e);
        }
    }

    async fn add_topup_seconds(&self, user_id: i64, seconds: i32) {
        if let Err(e) = sqlx::query(
            "INSERT INTO subscriptions (user_id, tier, topup_seconds_available, last_topup_at, updated_at) \
             VALUES ($1, 'free', $2, NOW(), NOW()) \
             ON CONFLICT (user_id) DO UPDATE SET \
               topup_seconds_available = subscriptions.topup_seconds_available + $2, \
               last_topup_at = NOW(), updated_at = NOW()",
        )
        .bind(user_id)
        .bind(seconds)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to add topup_seconds for {}: {}", user_id, e);
        }
    }

    async fn record_premium_usage(
        &self,
        user_id: i64,
        feature: &str,
        source_url: &str,
        duration_secs: i32,
        units: f64,
        cost_usd: f64,
    ) {
        if let Err(e) = sqlx::query(
            "INSERT INTO premium_usage (user_id, feature, source_url, duration_secs, units, estimated_cost_usd) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(user_id)
        .bind(feature)
        .bind(source_url)
        .bind(duration_secs)
        .bind(units as f32)
        .bind(cost_usd as f32) // DB column is REAL (f32); precision loss is acceptable for cost tracking
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to record premium usage for {}: {}", user_id, e);
        }
    }

    async fn store_callback_context(&self, ctx: &CallbackContext) -> i32 {
        let result: Result<(i32,), _> = sqlx::query_as(
            "INSERT INTO callback_contexts (source_url, chat_id, has_video, media_duration_secs, audio_cache_path) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(&ctx.source_url)
        .bind(ctx.chat_id)
        .bind(ctx.has_video)
        .bind(ctx.media_duration_secs)
        .bind(&ctx.audio_cache_path)
        .fetch_one(&self.pool)
        .await;

        match result {
            Ok((id,)) => id,
            Err(e) => {
                log::error!("Failed to store callback context: {}", e);
                0
            }
        }
    }

    async fn get_callback_context(&self, context_id: i32) -> Option<CallbackContext> {
        let row: Option<(String, i64, bool, Option<i32>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT source_url, chat_id, has_video, media_duration_secs, audio_cache_path, \
             transcript, transcript_language \
             FROM callback_contexts WHERE id = $1",
        )
        .bind(context_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            log::error!("Failed to get callback context {}: {}", context_id, e);
            e
        })
        .ok()
        .flatten();

        row.map(|(source_url, chat_id, has_video, media_duration_secs, audio_cache_path, transcript, transcript_language)| {
            CallbackContext {
                source_url,
                chat_id,
                has_video,
                media_duration_secs,
                audio_cache_path,
                transcript,
                transcript_language,
            }
        })
    }

    async fn cache_transcript(&self, context_id: i32, transcript: &str, language: Option<&str>) {
        if let Err(e) = sqlx::query(
            "UPDATE callback_contexts SET transcript = $1, transcript_language = $2 WHERE id = $3",
        )
        .bind(transcript)
        .bind(language)
        .bind(context_id)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to cache transcript for context {}: {}", context_id, e);
        }
    }

    async fn revoke_subscription(&self, user_id: i64) {
        if let Err(e) = sqlx::query(
            "UPDATE subscriptions SET tier = 'free', ai_seconds_limit = 0, \
             expires_at = NULL, updated_at = NOW() WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to revoke subscription for {}: {}", user_id, e);
        }
    }

    async fn revoke_topup(&self, user_id: i64, seconds: i32) {
        if let Err(e) = sqlx::query(
            "UPDATE subscriptions SET \
               topup_seconds_available = GREATEST(topup_seconds_available - $2, 0), \
               updated_at = NOW() \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(seconds)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to revoke topup for {}: {}", user_id, e);
        }
    }

    async fn get_latest_payment(&self, user_id: i64) -> Option<PaymentRecord> {
        let row: Option<(String, String, i32, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT telegram_payment_charge_id, product, amount, created_at \
             FROM payments WHERE user_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            log::error!("Failed to get latest payment for {}: {}", user_id, e);
            e
        })
        .ok()
        .flatten();

        row.map(|(telegram_charge_id, product, amount, created_at)| PaymentRecord {
            telegram_charge_id,
            product,
            amount,
            created_at,
        })
    }

    async fn get_recent_payments(&self, user_id: i64, limit: i64) -> Vec<PaymentRecord> {
        let rows: Result<Vec<(String, String, i32, chrono::DateTime<chrono::Utc>)>, _> =
            sqlx::query_as(
                "SELECT telegram_payment_charge_id, product, amount, created_at \
                 FROM payments WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2",
            )
            .bind(user_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await;

        match rows {
            Ok(rows) => rows
                .into_iter()
                .map(|(telegram_charge_id, product, amount, created_at)| PaymentRecord {
                    telegram_charge_id,
                    product,
                    amount,
                    created_at,
                })
                .collect(),
            Err(e) => {
                log::error!("Failed to get recent payments for {}: {}", user_id, e);
                vec![]
            }
        }
    }

    async fn has_ai_usage_since(&self, user_id: i64, since: chrono::DateTime<chrono::Utc>) -> bool {
        let result: Result<(bool,), _> = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM premium_usage WHERE user_id = $1 AND created_at > $2)",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await;

        match result {
            Ok((exists,)) => exists,
            Err(e) => {
                log::error!("Failed to check ai_usage_since for {}: {}", user_id, e);
                // Fail safe: assume usage exists so we don't accidentally auto-refund
                true
            }
        }
    }

    async fn cleanup_expired_callback_contexts(&self) {
        let result = sqlx::query(
            "DELETE FROM callback_contexts WHERE created_at < NOW() - INTERVAL '24 hours'",
        )
        .execute(&self.pool)
        .await;
        match result {
            Ok(r) => log::info!(
                "Callback context cleanup: removed {} expired entries",
                r.rows_affected()
            ),
            Err(e) => log::error!("Callback context cleanup failed: {}", e),
        }
    }

    async fn expire_stale_topups(&self) {
        let result = sqlx::query(
            "UPDATE subscriptions SET topup_seconds_available = 0, updated_at = NOW() \
             WHERE last_topup_at < NOW() - make_interval(days => $1::int) \
               AND topup_seconds_available > 0",
        )
        .bind(crate::terms::TOPUP_EXPIRY_DAYS)
        .execute(&self.pool)
        .await;
        match result {
            Ok(r) => log::info!("Expired {} stale top-up balances", r.rows_affected()),
            Err(e) => log::error!("Failed to expire stale top-ups: {}", e),
        }
    }
}
