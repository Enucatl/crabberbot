use serde_json::Value;
use sqlx::PgPool;

use crate::storage::{PaymentOrder, PostgresStorage, Storage};

#[derive(Debug)]
struct PaymentEvent {
    kind: &'static str,
    charge_id: String,
    user_id: i64,
    provider_charge_id: Option<String>,
    product: Option<String>,
    amount: Option<i32>,
    order: PaymentOrder,
}

fn payment_event(update: &Value) -> Result<Option<PaymentEvent>, &'static str> {
    let Some(message) = update.get("message") else {
        return Ok(None);
    };
    let (kind, payment) = if let Some(payment) = message.get("successful_payment") {
        ("payment", payment)
    } else if let Some(payment) = message.get("refunded_payment") {
        ("refund", payment)
    } else {
        return Ok(None);
    };
    let paid_at = message["date"]
        .as_i64()
        .and_then(|date| chrono::DateTime::from_timestamp(date, 0))
        .ok_or("invalid payment date")?;
    let update_id = update["update_id"]
        .as_u64()
        .and_then(|id| i64::try_from(id).ok())
        .ok_or("invalid payment update ID")?;
    let charge_id = payment["telegram_payment_charge_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("missing Telegram charge ID")?;
    let user_id = message["from"]["id"]
        .as_i64()
        .or_else(|| message["chat"]["id"].as_i64())
        .ok_or("missing payment user ID")?;
    let (provider_charge_id, product, amount) = if kind == "payment" {
        let product = payment["invoice_payload"]
            .as_str()
            .ok_or("missing payment product")?;
        let provider = payment["provider_payment_charge_id"]
            .as_str()
            .ok_or("missing provider charge ID")?;
        let amount = payment["total_amount"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or("invalid payment amount")?;
        (
            Some(provider.to_owned()),
            Some(product.to_owned()),
            Some(amount),
        )
    } else {
        (None, None, None)
    };
    Ok(Some(PaymentEvent {
        kind,
        charge_id: charge_id.to_owned(),
        user_id,
        provider_charge_id,
        product,
        amount,
        order: PaymentOrder { paid_at, update_id },
    }))
}

/// Called before the webhook returns 200. A failed write makes Telegram retry the update.
pub async fn record_update(pool: &PgPool, update: &Value) -> Result<(), sqlx::Error> {
    let event = payment_event(update).map_err(|error| sqlx::Error::Protocol(error.into()))?;
    let Some(event) = event else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO payment_inbox (kind, telegram_charge_id, user_id, provider_charge_id, product, amount, paid_at, telegram_update_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (kind, telegram_charge_id) DO NOTHING",
    )
    .bind(event.kind)
    .bind(&event.charge_id)
    .bind(event.user_id)
    .bind(event.provider_charge_id)
    .bind(event.product)
    .bind(event.amount)
    .bind(event.order.paid_at)
    .bind(event.order.update_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replays financial state independently of Teloxide's in-memory dispatcher.
pub async fn replay_pending(pool: &PgPool) -> Result<(), sqlx::Error> {
    type InboxRow = (
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<i32>,
        chrono::DateTime<chrono::Utc>,
        i64,
    );
    let events: Vec<InboxRow> = sqlx::query_as(
        "SELECT id, kind, telegram_charge_id, user_id, provider_charge_id, product, amount, paid_at, telegram_update_id \
             FROM payment_inbox WHERE processed_at IS NULL ORDER BY paid_at, telegram_update_id, id",
    )
    .fetch_all(pool)
    .await?;
    let storage = PostgresStorage::new(pool.clone());
    for (id, kind, charge_id, user_id, provider, product, amount, paid_at, update_id) in events {
        let result = match kind.as_str() {
            "payment" => {
                storage
                    .fulfill_payment(
                        user_id,
                        &charge_id,
                        provider.as_deref().unwrap_or_default(),
                        product.as_deref().unwrap_or_default(),
                        amount.unwrap_or_default(),
                        PaymentOrder { paid_at, update_id },
                    )
                    .await
            }
            "refund" => storage.refund_payment(user_id, &charge_id).await,
            _ => continue,
        };
        match result {
            Ok(true) => log::info!("Replayed {} for Telegram charge {}", kind, charge_id),
            Ok(false) if kind == "refund" => {
                let refunded: Option<(bool,)> = sqlx::query_as(
                    "SELECT refunded_at IS NOT NULL FROM payments \
                     WHERE user_id = $1 AND telegram_payment_charge_id = $2",
                )
                .bind(user_id)
                .bind(&charge_id)
                .fetch_optional(pool)
                .await?;
                if refunded != Some((true,)) {
                    continue; // Payment has not been fulfilled yet; retry after it arrives.
                }
            }
            Ok(false) => {}
            Err(error) => {
                log::error!(
                    "Cannot replay {} for Telegram charge {}: {}",
                    kind,
                    charge_id,
                    error
                );
                continue;
            }
        }
        sqlx::query("UPDATE payment_inbox SET processed_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{payment_event, record_update, replay_pending};
    use crate::storage::{PaymentOrder, PostgresStorage, Storage};
    use crate::subscription::SubscriptionTier;
    use serde_json::json;
    use sqlx::PgPool;

    fn update(kind: &str, charge: &str) -> serde_json::Value {
        let mut update = json!({"update_id": 1, "message": {
            "date": chrono::Utc::now().timestamp(), "from": {"id": 42}, "chat": {"id": 42}
        }});
        update["message"][kind] = json!({
            "telegram_payment_charge_id": charge,
            "provider_payment_charge_id": "provider",
            "invoice_payload": "topup_60",
            "total_amount": 50
        });
        update
    }

    #[test]
    fn parses_payment_and_refund() {
        let payment = payment_event(&update("successful_payment", "charge"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (payment.kind, payment.user_id, payment.amount),
            ("payment", 42, Some(50))
        );
        assert_eq!(
            payment_event(&update("refunded_payment", "charge"))
                .unwrap()
                .unwrap()
                .kind,
            "refund"
        );
        let mut invalid = update("successful_payment", "charge");
        invalid["message"]["date"] = json!("invalid");
        assert!(payment_event(&invalid).is_err());
        invalid["message"]["date"] = json!(0);
        invalid["update_id"] = json!(-1);
        assert!(payment_event(&invalid).is_err());
    }

    #[sqlx::test(migrations = false)]
    async fn migration_preserves_legacy_pending_purchase_order(pool: PgPool) {
        for migration in sqlx::migrate!("./migrations")
            .iter()
            .filter(|m| m.version < 16)
        {
            sqlx::raw_sql(migration.sql.clone())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO payment_inbox (kind, telegram_charge_id, user_id, provider_charge_id, product, amount) \
             VALUES ('payment', 'legacy-basic', 42, 'provider', 'sub_basic', 50), \
                    ('payment', 'legacy-pro', 42, 'provider', 'sub_pro', 150)"
        ).execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/016_payment_order.sql"))
            .execute(&pool)
            .await
            .unwrap();

        replay_pending(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let storage = PostgresStorage::new(pool);
        assert_eq!(
            storage.get_subscription(42).await.tier,
            SubscriptionTier::Pro
        );
        assert_eq!(
            storage
                .get_latest_payment(42)
                .await
                .unwrap()
                .telegram_charge_id,
            "legacy-pro"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replayed_topups_preserve_latest_expiry(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        let now = chrono::Utc::now().timestamp();
        let mut newer = update("successful_payment", "newer-topup");
        newer["message"]["date"] = json!(now);
        record_update(&pool, &newer).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let mut older = update("successful_payment", "older-topup");
        older["message"]["date"] = json!(now - 60);
        record_update(&pool, &older).await.unwrap();
        replay_pending(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let sub = storage.get_subscription(42).await;
        assert_eq!(
            sub.topup_seconds_available,
            2 * crate::subscription::TOPUP_SECONDS
        );
        assert_eq!(sub.last_topup_at.unwrap().timestamp(), now);

        // Migration 016 uses the epoch for old inbox events whose date was lost.
        let mut legacy = update("successful_payment", "legacy-topup");
        legacy["message"]["from"]["id"] = json!(43);
        legacy["message"]["date"] = json!(0);
        record_update(&pool, &legacy).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let sub = storage.get_subscription(43).await;
        assert_eq!(
            sub.topup_seconds_available,
            crate::subscription::TOPUP_SECONDS
        );
        assert!(sub.last_topup_at.unwrap().timestamp() >= now);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replay_preserves_newer_purchases_refunds_and_grants(pool: PgPool) {
        let storage = PostgresStorage::new(pool.clone());
        let now = chrono::Utc::now().timestamp();
        // Cover equal-second purchases and a reset Telegram sequence on a later date.
        for (user_id, old_date, old_id, new_id) in [(42, now, 1, 2), (43, now - 86400, 900, 1)] {
            let old_charge = format!("old-{user_id}");
            let new_charge = format!("new-{user_id}");
            let mut old = update("successful_payment", &old_charge);
            old["update_id"] = json!(old_id);
            old["message"]["date"] = json!(old_date);
            old["message"]["from"]["id"] = json!(user_id);
            old["message"]["successful_payment"]["invoice_payload"] = json!("sub_basic");
            record_update(&pool, &old).await.unwrap();
            sqlx::query("ALTER TABLE subscriptions ADD CONSTRAINT reject_basic CHECK (tier <> 'basic') NOT VALID")
                .execute(&pool).await.unwrap();
            replay_pending(&pool).await.unwrap();
            sqlx::query("ALTER TABLE subscriptions DROP CONSTRAINT reject_basic")
                .execute(&pool)
                .await
                .unwrap();

            let new_order = PaymentOrder {
                paid_at: chrono::DateTime::from_timestamp(now, 0).unwrap(),
                update_id: new_id,
            };
            storage
                .fulfill_payment(user_id, &new_charge, "provider", "sub_pro", 150, new_order)
                .await
                .unwrap();
            storage.reserve_ai_seconds(user_id, 60).await.unwrap();
            let expiry = storage.get_subscription(user_id).await.expires_at;
            replay_pending(&pool).await.unwrap();
            replay_pending(&pool).await.unwrap();
            let sub = storage.get_subscription(user_id).await;
            assert_eq!(
                (sub.tier, sub.ai_seconds_used, sub.expires_at),
                (SubscriptionTier::Pro, 60, expiry)
            );
            let latest = storage.get_latest_payment(user_id).await.unwrap();
            assert_eq!(latest.telegram_charge_id, new_charge);
            assert_eq!(latest.created_at, new_order.paid_at);
            let recent = storage.get_recent_payments(user_id, 2).await;
            assert_eq!(recent[1].telegram_charge_id, old_charge);
            assert_eq!(recent[1].created_at.timestamp(), old_date);
            storage.refund_payment(user_id, &old_charge).await.unwrap();
            assert_eq!(
                storage.get_subscription(user_id).await.tier,
                SubscriptionTier::Pro
            );

            storage.refund_payment(user_id, &new_charge).await.unwrap();
            // A different stale charge must not resurrect access after the newer refund.
            old["message"]["successful_payment"]["telegram_payment_charge_id"] =
                json!(format!("stale-refund-{user_id}"));
            record_update(&pool, &old).await.unwrap();
            replay_pending(&pool).await.unwrap();
            assert_eq!(
                storage.get_subscription(user_id).await.tier,
                SubscriptionTier::Free
            );

            let grant_order = PaymentOrder {
                update_id: new_id + 1,
                ..new_order
            };
            storage
                .upsert_subscription(user_id, SubscriptionTier::Pro, 30, grant_order)
                .await;
            old["message"]["successful_payment"]["telegram_payment_charge_id"] =
                json!(format!("stale-grant-{user_id}"));
            record_update(&pool, &old).await.unwrap();
            replay_pending(&pool).await.unwrap();
            assert_eq!(
                storage.get_subscription(user_id).await.tier,
                SubscriptionTier::Pro
            );
            // A genuinely later purchase in the same second still supersedes the grant.
            storage
                .fulfill_payment(
                    user_id,
                    &format!("after-grant-{user_id}"),
                    "provider",
                    "sub_basic",
                    50,
                    PaymentOrder {
                        update_id: new_id + 2,
                        ..new_order
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                storage.get_subscription(user_id).await.tier,
                SubscriptionTier::Basic
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replays_after_fulfillment_failure_once(pool: PgPool) {
        record_update(&pool, &update("successful_payment", "charge"))
            .await
            .unwrap();
        record_update(&pool, &update("successful_payment", "charge"))
            .await
            .unwrap();
        let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payment_inbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(event_count, 1);
        sqlx::query("ALTER TABLE subscriptions ADD CONSTRAINT reject_topup CHECK (topup_seconds_available = 0)")
            .execute(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM payment_inbox WHERE processed_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 1);
        sqlx::query("ALTER TABLE subscriptions DROP CONSTRAINT reject_topup")
            .execute(&pool)
            .await
            .unwrap();
        replay_pending(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let balance: i32 = sqlx::query_scalar(
            "SELECT topup_seconds_available FROM subscriptions WHERE user_id = 42",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(balance, crate::subscription::TOPUP_SECONDS);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refund_waits_for_payment_and_applies_once(pool: PgPool) {
        record_update(&pool, &update("refunded_payment", "charge"))
            .await
            .unwrap();
        replay_pending(&pool).await.unwrap();
        record_update(&pool, &update("successful_payment", "charge"))
            .await
            .unwrap();
        replay_pending(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        replay_pending(&pool).await.unwrap();
        let (balance, refunded): (i32, bool) = sqlx::query_as(
            "SELECT s.topup_seconds_available, p.refunded_at IS NOT NULL FROM subscriptions s \
             JOIN payments p ON p.user_id = s.user_id WHERE p.telegram_payment_charge_id = 'charge'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!((balance, refunded), (0, true));
    }
}
