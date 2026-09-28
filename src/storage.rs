use async_trait::async_trait;
use sqlx::PgPool;

use crate::downloader::MediaType;
use crate::handler::CallbackContext;
use crate::subscription::{SubscriptionInfo, SubscriptionTier};

/// A payment record returned for self-service refund eligibility checks and owner tooling.
#[derive(Debug, Clone)]
pub struct PaymentRecord {
    pub telegram_charge_id: String,
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

/// The exact credit split removed before an AI action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaReservation {
    pub monthly_seconds: i32,
    pub topup_seconds: i32,
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
    async fn upsert_subscription(&self, user_id: i64, tier: SubscriptionTier, duration_days: i64);

    /// Records a charge and grants its product entitlement atomically.
    /// Returns Ok(true) only when this charge was fulfilled for the first time.
    async fn fulfill_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
        provider_charge_id: &str,
        product: &str,
        amount: i32,
    ) -> Result<bool, sqlx::Error>;

    // AI Seconds tracking
    async fn consume_ai_seconds(&self, user_id: i64, seconds: i32);
    async fn reserve_ai_seconds(&self, user_id: i64, seconds: i32) -> Option<QuotaReservation>;
    async fn release_ai_seconds(&self, user_id: i64, reservation: QuotaReservation);
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
    async fn cache_transcript(
        &self,
        context_id: i32,
        transcript: &str,
        language: Option<String>,
        speaker_count: Option<i32>,
    );
    async fn cache_ai_result(
        &self,
        context_id: i32,
        transcript: &str,
        language: &str,
        summary: &str,
        speaker_count: i32,
    );

    /// Records a payment refund and revokes its entitlement atomically.
    /// Returns Ok(true) only when the stored charge was refunded for the first time.
    async fn refund_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
    ) -> Result<bool, sqlx::Error>;
    /// Returns the most recent unrefunded payment for a user, if any.
    async fn get_latest_payment(&self, user_id: i64) -> Option<PaymentRecord>;
    /// Returns the most recent `limit` unrefunded payments for a user (for owner tooling).
    async fn get_recent_payments(&self, user_id: i64, limit: i64) -> Vec<PaymentRecord>;
    /// Returns true if transcription or summarization was used after `since`.
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
        let result: Result<Vec<(Option<String>,)>, _> = sqlx::query_as(
            "DELETE FROM media_cache WHERE last_used_at < NOW() - make_interval(days => $1::int) \
             RETURNING audio_cache_path",
        )
        .bind(ttl_days)
        .fetch_all(pool)
        .await;

        match result {
            Ok(rows) => {
                log::info!("Cache cleanup: removed {} expired entries", rows.len());
                for path in rows.into_iter().filter_map(|(p,)| p) {
                    let referenced: Result<(bool,), _> = sqlx::query_as(
                        "SELECT EXISTS (SELECT 1 FROM media_cache WHERE audio_cache_path = $1 \
                         UNION ALL SELECT 1 FROM callback_contexts \
                         WHERE audio_cache_path = $1 AND created_at >= NOW() - INTERVAL '24 hours')",
                    )
                    .bind(&path)
                    .fetch_one(pool)
                    .await;
                    match referenced {
                        Ok((true,)) => continue,
                        Err(e) => {
                            log::warn!("Cannot check audio references for {}: {}", path, e);
                            continue;
                        }
                        Ok((false,)) => {}
                    }
                    if let Err(e) = tokio::fs::remove_file(&path).await
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        log::warn!("Failed to delete expired audio file {}: {}", path, e);
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
        let cache_row: Option<(i32, String, Option<String>, Option<i32>)> = sqlx::query_as(
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

        Some(CachedMedia {
            caption,
            files,
            audio_cache_path,
            media_duration_secs,
        })
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
            log::error!(
                "Failed to delete old cached files for {}: {}",
                source_url,
                e
            );
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
        let row: Option<(
            String,
            i32,
            i32,
            i32,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        )> = sqlx::query_as(
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

    async fn upsert_subscription(&self, user_id: i64, tier: SubscriptionTier, duration_days: i64) {
        let limit = tier.ai_seconds_limit();
        let tier_str = tier.to_string();
        if let Err(e) = sqlx::query(
            "INSERT INTO subscriptions (user_id, tier, ai_seconds_used, ai_seconds_limit, expires_at, source_payment_id, updated_at) \
             VALUES ($1, $2, 0, $3, NOW() + make_interval(days => $4::int), NULL, NOW()) \
             ON CONFLICT (user_id) DO UPDATE SET \
               tier = $2, ai_seconds_used = 0, ai_seconds_limit = $3, \
               expires_at = NOW() + make_interval(days => $4::int), \
               source_payment_id = NULL, updated_at = NOW()",
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

    async fn fulfill_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
        provider_charge_id: &str,
        product: &str,
        amount: i32,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let inserted: Option<(i32,)> = sqlx::query_as(
            "INSERT INTO payments (user_id, telegram_payment_charge_id, provider_payment_charge_id, product, amount) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (telegram_payment_charge_id) DO NOTHING RETURNING id",
        )
        .bind(user_id)
        .bind(telegram_charge_id)
        .bind(provider_charge_id)
        .bind(product)
        .bind(amount)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((payment_id,)) = inserted else {
            return Ok(false);
        };

        let result = match product {
            crate::subscription::PRODUCT_SUB_BASIC | crate::subscription::PRODUCT_SUB_PRO => {
                let tier = if product == crate::subscription::PRODUCT_SUB_BASIC {
                    SubscriptionTier::Basic
                } else {
                    SubscriptionTier::Pro
                };
                sqlx::query(
                    "INSERT INTO subscriptions (user_id, tier, ai_seconds_used, ai_seconds_limit, expires_at, source_payment_id, updated_at) \
                     VALUES ($1, $2, 0, $3, NOW() + make_interval(days => 30), $4, NOW()) \
                     ON CONFLICT (user_id) DO UPDATE SET \
                       tier = $2, ai_seconds_used = 0, ai_seconds_limit = $3, \
                       expires_at = NOW() + make_interval(days => 30), \
                       source_payment_id = $4, updated_at = NOW()",
                )
                .bind(user_id)
                .bind(tier.to_string())
                .bind(tier.ai_seconds_limit())
                .bind(payment_id)
                .execute(&mut *tx)
                .await
            }
            crate::subscription::PRODUCT_TOPUP_60 => sqlx::query(
                "INSERT INTO subscriptions (user_id, tier, topup_seconds_available, last_topup_at, updated_at) \
                 VALUES ($1, 'free', $2, NOW(), NOW()) \
                 ON CONFLICT (user_id) DO UPDATE SET \
                   topup_seconds_available = subscriptions.topup_seconds_available + $2, \
                   last_topup_at = NOW(), updated_at = NOW()",
            )
            .bind(user_id)
            .bind(crate::subscription::TOPUP_SECONDS)
            .execute(&mut *tx)
            .await,
            _ => return Err(sqlx::Error::Protocol("Unknown payment product".into())),
        };
        result?;
        tx.commit().await?;
        Ok(true)
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

    async fn reserve_ai_seconds(&self, user_id: i64, seconds: i32) -> Option<QuotaReservation> {
        if seconds <= 0 {
            return None;
        }
        let row: Option<(i32, i32)> = sqlx::query_as(
            "WITH reservation AS ( \
               SELECT user_id, \
                 LEAST($2, CASE WHEN tier <> 'free' AND expires_at > NOW() THEN GREATEST(ai_seconds_limit - ai_seconds_used, 0) ELSE 0 END) AS monthly, \
                 GREATEST($2 - LEAST($2, CASE WHEN tier <> 'free' AND expires_at > NOW() THEN GREATEST(ai_seconds_limit - ai_seconds_used, 0) ELSE 0 END), 0) AS topup \
               FROM subscriptions \
               WHERE user_id = $1 AND \
                 (CASE WHEN tier <> 'free' AND expires_at > NOW() THEN GREATEST(ai_seconds_limit - ai_seconds_used, 0) ELSE 0 END) + \
                   CASE WHEN last_topup_at IS NULL OR last_topup_at > NOW() - INTERVAL '365 days' \
                     THEN topup_seconds_available ELSE 0 END >= $2 \
               FOR UPDATE \
             ) \
             UPDATE subscriptions s SET ai_seconds_used = s.ai_seconds_used + r.monthly, \
                 topup_seconds_available = s.topup_seconds_available - r.topup, updated_at = NOW() \
             FROM reservation r WHERE s.user_id = r.user_id \
             RETURNING r.monthly, r.topup",
        )
        .bind(user_id)
        .bind(seconds)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        row.map(|(monthly_seconds, topup_seconds)| QuotaReservation {
            monthly_seconds,
            topup_seconds,
        })
    }

    async fn release_ai_seconds(&self, user_id: i64, reservation: QuotaReservation) {
        if reservation.monthly_seconds <= 0 && reservation.topup_seconds <= 0 {
            return;
        }
        if let Err(e) = sqlx::query(
            "UPDATE subscriptions SET ai_seconds_used = GREATEST(ai_seconds_used - $2, 0), \
             topup_seconds_available = topup_seconds_available + $3, updated_at = NOW() WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(reservation.monthly_seconds)
        .bind(reservation.topup_seconds)
        .execute(&self.pool)
        .await
        {
            log::error!("Failed to release AI reservation for {}: {}", user_id, e);
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
            "INSERT INTO callback_contexts (source_url, chat_id, user_id, has_video, media_duration_secs, audio_cache_path) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(&ctx.source_url)
        .bind(ctx.chat_id)
        .bind(ctx.user_id)
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
        let row: Option<(
            String,
            i64,
            i64,
            bool,
            Option<i32>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i32>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT source_url, chat_id, user_id, has_video, media_duration_secs, audio_cache_path, \
             transcript, transcript_language, summary, speaker_count, raw_transcript \
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

        row.map(
            |(
                source_url,
                chat_id,
                user_id,
                has_video,
                media_duration_secs,
                audio_cache_path,
                transcript,
                transcript_language,
                summary,
                speaker_count,
                raw_transcript,
            )| {
                CallbackContext {
                    source_url,
                    chat_id,
                    user_id,
                    has_video,
                    media_duration_secs,
                    audio_cache_path,
                    transcript,
                    transcript_language,
                    summary,
                    speaker_count,
                    raw_transcript,
                }
            },
        )
    }

    async fn cache_transcript(
        &self,
        context_id: i32,
        transcript: &str,
        language: Option<String>,
        speaker_count: Option<i32>,
    ) {
        if let Err(e) = sqlx::query(
            "UPDATE callback_contexts SET raw_transcript = $1, transcript_language = $2, speaker_count = $3 WHERE id = $4",
        )
        .bind(transcript)
        .bind(language)
        .bind(speaker_count)
        .bind(context_id)
        .execute(&self.pool)
        .await
        {
            log::error!(
                "Failed to cache transcript for context {}: {}",
                context_id,
                e
            );
        }
    }

    async fn cache_ai_result(
        &self,
        context_id: i32,
        transcript: &str,
        language: &str,
        summary: &str,
        speaker_count: i32,
    ) {
        if let Err(e) = sqlx::query(
            "UPDATE callback_contexts SET transcript = $1, transcript_language = $2, summary = $3, speaker_count = $4 WHERE id = $5",
        )
        .bind(transcript)
        .bind(language)
        .bind(summary)
        .bind(speaker_count)
        .bind(context_id)
        .execute(&self.pool)
        .await
        {
            log::error!(
                "Failed to cache AI result for context {}: {}",
                context_id,
                e
            );
        }
    }

    async fn refund_payment(
        &self,
        user_id: i64,
        telegram_charge_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let product: Option<(i32, String)> = sqlx::query_as(
            "UPDATE payments SET refunded_at = NOW() \
             WHERE user_id = $1 AND telegram_payment_charge_id = $2 AND refunded_at IS NULL \
             RETURNING id, product",
        )
        .bind(user_id)
        .bind(telegram_charge_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((payment_id, product)) = product else {
            return Ok(false);
        };

        let result = match product.as_str() {
            crate::subscription::PRODUCT_SUB_BASIC | crate::subscription::PRODUCT_SUB_PRO => {
                sqlx::query(
                    "UPDATE subscriptions SET tier = 'free', ai_seconds_limit = 0, \
                     expires_at = NULL, source_payment_id = NULL, updated_at = NOW() \
                     WHERE user_id = $1 AND source_payment_id = $2",
                )
                .bind(user_id)
                .bind(payment_id)
                .execute(&mut *tx)
                .await
            }
            crate::subscription::PRODUCT_TOPUP_60 => {
                sqlx::query(
                    "UPDATE subscriptions SET \
                   topup_seconds_available = GREATEST(topup_seconds_available - $2, 0), \
                   updated_at = NOW() WHERE user_id = $1",
                )
                .bind(user_id)
                .bind(crate::subscription::TOPUP_SECONDS)
                .execute(&mut *tx)
                .await
            }
            _ => Ok(sqlx::postgres::PgQueryResult::default()),
        };
        result?;
        tx.commit().await?;
        Ok(true)
    }

    async fn get_latest_payment(&self, user_id: i64) -> Option<PaymentRecord> {
        let row: Option<(String, i32, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT telegram_payment_charge_id, amount, created_at \
             FROM payments WHERE user_id = $1 AND refunded_at IS NULL ORDER BY created_at DESC LIMIT 1",
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

        row.map(|(telegram_charge_id, amount, created_at)| PaymentRecord {
            telegram_charge_id,
            amount,
            created_at,
        })
    }

    async fn get_recent_payments(&self, user_id: i64, limit: i64) -> Vec<PaymentRecord> {
        let rows: Result<Vec<(String, i32, chrono::DateTime<chrono::Utc>)>, _> =
            sqlx::query_as(
                "SELECT telegram_payment_charge_id, amount, created_at \
                 FROM payments WHERE user_id = $1 AND refunded_at IS NULL ORDER BY created_at DESC LIMIT $2",
            )
            .bind(user_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await;

        match rows {
            Ok(rows) => rows
                .into_iter()
                .map(|(telegram_charge_id, amount, created_at)| PaymentRecord {
                    telegram_charge_id,
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
            "SELECT EXISTS(SELECT 1 FROM premium_usage WHERE user_id = $1 AND created_at > $2 \
             AND feature IN ('transcribe', 'summarize', 'openrouter_transcript_summary_input', \
                             'openrouter_transcript_summary_output'))",
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

#[cfg(test)]
mod tests {
    use super::{PostgresStorage, QuotaReservation, Storage};
    use crate::subscription::{
        PRODUCT_SUB_BASIC, PRODUCT_SUB_PRO, PRODUCT_TOPUP_60, SubscriptionTier, TOPUP_SECONDS,
    };
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn cleanup_keeps_audio_when_row_is_refreshed_during_delete(pool: PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.mp3");
        tokio::fs::write(&path, b"audio").await.unwrap();
        sqlx::query(
            "INSERT INTO media_cache (source_url, caption, audio_cache_path, last_used_at) \
             VALUES ('refreshed', '', $1, NOW() - INTERVAL '8 days')",
        )
        .bind(path.to_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM media_cache WHERE source_url = 'refreshed' FOR UPDATE")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let cleanup_pool = pool.clone();
        let cleanup = tokio::spawn(async move {
            PostgresStorage::cleanup_expired(&cleanup_pool, 7).await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock' \
                     AND query LIKE 'DELETE FROM media_cache WHERE last_used_at%')",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cleanup did not reach the locked row");
        sqlx::query("UPDATE media_cache SET last_used_at = NOW() WHERE source_url = 'refreshed'")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        cleanup.await.unwrap();

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM media_cache WHERE source_url = 'refreshed'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        assert!(path.exists());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cleanup_retains_callback_audio_and_releases_unreferenced_audio(pool: PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let callback_path = dir.path().join("callback.mp3");
        let unreferenced_path = dir.path().join("unreferenced.mp3");
        tokio::fs::write(&callback_path, b"audio").await.unwrap();
        tokio::fs::write(&unreferenced_path, b"audio")
            .await
            .unwrap();
        for (source, path) in [
            ("callback", &callback_path),
            ("unreferenced", &unreferenced_path),
        ] {
            sqlx::query(
                "INSERT INTO media_cache (source_url, caption, audio_cache_path, last_used_at) \
                 VALUES ($1, '', $2, NOW() - INTERVAL '8 days')",
            )
            .bind(source)
            .bind(path.to_str().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO callback_contexts (source_url, chat_id, audio_cache_path) \
             VALUES ('callback', 1, $1)",
        )
        .bind(callback_path.to_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

        PostgresStorage::cleanup_expired(&pool, 7).await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert!(callback_path.exists());
        assert!(!unreferenced_path.exists());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_basic_charge_grants_once(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());

        assert!(
            storage
                .fulfill_payment(1, "basic-charge", "provider-charge", PRODUCT_SUB_BASIC, 50)
                .await
                .unwrap()
        );
        assert!(
            !storage
                .fulfill_payment(1, "basic-charge", "provider-charge", PRODUCT_SUB_BASIC, 50)
                .await
                .unwrap()
        );

        let payment_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (tier, used, limit, expires_at): (String, i32, i32, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as(
                "SELECT tier, ai_seconds_used, ai_seconds_limit, expires_at \
                 FROM subscriptions WHERE user_id = 1",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(payment_count, 1);
        assert_eq!((tier.as_str(), used, limit), ("basic", 0, 3600));
        assert!(expires_at > chrono::Utc::now() + chrono::TimeDelta::days(29));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_topup_charge_grants_once(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());

        assert!(
            storage
                .fulfill_payment(2, "topup-charge", "provider-charge", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );
        assert!(
            !storage
                .fulfill_payment(2, "topup-charge", "provider-charge", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );

        let payment_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        let topup_seconds: i32 = sqlx::query_scalar(
            "SELECT topup_seconds_available FROM subscriptions WHERE user_id = 2",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(payment_count, 1);
        assert_eq!(topup_seconds, TOPUP_SECONDS);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_reservations_do_not_overspend(pool: PgPool) {
        let storage = std::sync::Arc::new(PostgresStorage::new(pool));
        storage
            .fulfill_payment(1, "reserve-charge", "provider", PRODUCT_SUB_BASIC, 50)
            .await
            .unwrap();
        let first = storage.clone();
        let second = storage.clone();
        let (left, right) = tokio::join!(
            async move { first.reserve_ai_seconds(1, 2_400).await },
            async move { second.reserve_ai_seconds(1, 2_400).await },
        );
        assert_eq!(
            usize::from(left.is_some()) + usize::from(right.is_some()),
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reservation_release_restores_the_original_split(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        sqlx::query("INSERT INTO subscriptions (user_id, tier, ai_seconds_limit, topup_seconds_available, expires_at) VALUES (1, 'basic', 100, 50, NOW() + INTERVAL '1 day')")
            .execute(&pool).await.unwrap();
        let reservation = storage.reserve_ai_seconds(1, 120).await.unwrap();
        assert_eq!(
            reservation,
            QuotaReservation {
                monthly_seconds: 100,
                topup_seconds: 20
            }
        );
        storage.release_ai_seconds(1, reservation).await;
        let row: (i32, i32) = sqlx::query_as(
            "SELECT ai_seconds_used, topup_seconds_available FROM subscriptions WHERE user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (0, 50));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn quota_constraints_reject_invalid_rows(pool: PgPool) {
        assert!(sqlx::query("INSERT INTO subscriptions (user_id, tier, ai_seconds_limit) VALUES (1, 'invalid', 0)").execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO payments (user_id, telegram_payment_charge_id, provider_payment_charge_id, product, amount) VALUES (1, 'charge', 'provider', 'topup_60', 0)").execute(&pool).await.is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_subscription_once_marks_payment(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        assert!(
            storage
                .fulfill_payment(1, "basic-charge", "provider-charge", PRODUCT_SUB_BASIC, 50)
                .await
                .unwrap()
        );

        assert!(storage.refund_payment(1, "basic-charge").await.unwrap());
        assert!(!storage.refund_payment(1, "basic-charge").await.unwrap());

        let (refunded_at, tier, limit): (Option<chrono::DateTime<chrono::Utc>>, String, i32) =
            sqlx::query_as(
                "SELECT p.refunded_at, s.tier, s.ai_seconds_limit FROM payments p \
                 JOIN subscriptions s ON s.user_id = p.user_id WHERE p.telegram_payment_charge_id = 'basic-charge'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(refunded_at.is_some());
        assert_eq!((tier.as_str(), limit), ("free", 0));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_only_revokes_its_own_subscription(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        storage
            .fulfill_payment(1, "basic-a", "provider-a", PRODUCT_SUB_BASIC, 50)
            .await
            .unwrap();
        storage
            .fulfill_payment(1, "pro-b", "provider-b", PRODUCT_SUB_PRO, 150)
            .await
            .unwrap();

        assert!(storage.refund_payment(1, "basic-a").await.unwrap());
        let (tier, limit, source): (String, i32, Option<String>) = sqlx::query_as(
            "SELECT s.tier, s.ai_seconds_limit, p.telegram_payment_charge_id \
             FROM subscriptions s LEFT JOIN payments p ON p.id = s.source_payment_id \
             WHERE s.user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (tier.as_str(), limit, source.as_deref()),
            ("pro", 12_000, Some("pro-b"))
        );

        assert!(storage.refund_payment(1, "pro-b").await.unwrap());
        let (tier, limit, source): (String, i32, Option<i32>) = sqlx::query_as(
            "SELECT tier, ai_seconds_limit, source_payment_id FROM subscriptions WHERE user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((tier.as_str(), limit, source), ("free", 0, None));

        storage
            .fulfill_payment(1, "basic-c", "provider-c", PRODUCT_SUB_BASIC, 50)
            .await
            .unwrap();
        storage
            .upsert_subscription(1, SubscriptionTier::Pro, 30)
            .await;
        assert!(storage.refund_payment(1, "basic-c").await.unwrap());
        let (tier, limit, source): (String, i32, Option<i32>) = sqlx::query_as(
            "SELECT tier, ai_seconds_limit, source_payment_id FROM subscriptions WHERE user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((tier.as_str(), limit, source), ("pro", 12_000, None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_storage_failure_is_not_a_duplicate(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        assert!(
            storage
                .fulfill_payment(1, "charge", "provider", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );
        sqlx::query(
            "ALTER TABLE payments ADD CONSTRAINT reject_refund CHECK (refunded_at IS NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(storage.refund_payment(1, "charge").await.is_err());
        sqlx::query("ALTER TABLE payments DROP CONSTRAINT reject_refund")
            .execute(&pool)
            .await
            .unwrap();
        assert!(storage.refund_payment(1, "charge").await.unwrap());
        assert!(!storage.refund_payment(1, "charge").await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_topup_once_and_rejects_wrong_charge_owner(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        assert!(
            storage
                .fulfill_payment(1, "topup-a", "provider-a", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );
        assert!(
            storage
                .fulfill_payment(1, "topup-b", "provider-b", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );

        assert!(!storage.refund_payment(2, "topup-a").await.unwrap());
        assert!(!storage.refund_payment(1, "unknown-charge").await.unwrap());
        assert!(storage.refund_payment(1, "topup-a").await.unwrap());
        assert!(!storage.refund_payment(1, "topup-a").await.unwrap());

        let topup_seconds: i32 = sqlx::query_scalar(
            "SELECT topup_seconds_available FROM subscriptions WHERE user_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(topup_seconds, TOPUP_SECONDS);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_payment_skips_refunded_charges(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        assert!(
            storage
                .fulfill_payment(1, "older", "provider-a", PRODUCT_SUB_BASIC, 50)
                .await
                .unwrap()
        );
        assert!(
            storage
                .fulfill_payment(1, "newer", "provider-b", PRODUCT_TOPUP_60, 50)
                .await
                .unwrap()
        );
        assert!(storage.refund_payment(1, "newer").await.unwrap());

        assert_eq!(
            storage
                .get_latest_payment(1)
                .await
                .map(|payment| payment.telegram_charge_id),
            Some("older".to_string())
        );
        assert_eq!(storage.get_recent_payments(1, 5).await.len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_usage_matches_transcription_and_summarization(pool: PgPool) {
        let storage = PostgresStorage::new(pool);
        let since = chrono::Utc::now() - chrono::TimeDelta::minutes(1);

        storage
            .record_premium_usage(1, "audio_extract", "video", 60, 0.0, 0.0)
            .await;
        assert!(!storage.has_ai_usage_since(1, since).await);

        for (user_id, feature) in [
            (2, "transcribe"),
            (3, "summarize"),
            (4, "openrouter_transcript_summary_input"),
            (5, "openrouter_transcript_summary_output"),
        ] {
            storage
                .record_premium_usage(user_id, feature, "video", 60, 0.0, 0.0)
                .await;
            assert!(
                storage.has_ai_usage_since(user_id, since).await,
                "{feature}"
            );
        }
    }
}
