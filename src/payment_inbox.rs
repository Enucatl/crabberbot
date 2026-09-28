use serde_json::Value;
use sqlx::PgPool;

use crate::storage::{PostgresStorage, Storage};

#[derive(Debug)]
struct PaymentEvent {
    kind: &'static str,
    charge_id: String,
    user_id: i64,
    provider_charge_id: Option<String>,
    product: Option<String>,
    amount: Option<i32>,
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
    }))
}

/// Called before the webhook returns 200. A failed write makes Telegram retry the update.
pub async fn record_update(pool: &PgPool, update: &Value) -> Result<(), sqlx::Error> {
    let event = payment_event(update).map_err(|error| sqlx::Error::Protocol(error.into()))?;
    let Some(event) = event else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO payment_inbox (kind, telegram_charge_id, user_id, provider_charge_id, product, amount) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (kind, telegram_charge_id) DO NOTHING",
    )
    .bind(event.kind)
    .bind(&event.charge_id)
    .bind(event.user_id)
    .bind(event.provider_charge_id)
    .bind(event.product)
    .bind(event.amount)
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
    );
    let events: Vec<InboxRow> = sqlx::query_as(
        "SELECT id, kind, telegram_charge_id, user_id, provider_charge_id, product, amount \
             FROM payment_inbox WHERE processed_at IS NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let storage = PostgresStorage::new(pool.clone());
    for (id, kind, charge_id, user_id, provider, product, amount) in events {
        let result = match kind.as_str() {
            "payment" => {
                storage
                    .fulfill_payment(
                        user_id,
                        &charge_id,
                        provider.as_deref().unwrap_or_default(),
                        product.as_deref().unwrap_or_default(),
                        amount.unwrap_or_default(),
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
    use serde_json::json;
    use sqlx::PgPool;

    fn update(kind: &str, charge: &str) -> serde_json::Value {
        let mut update = json!({"message": {"from": {"id": 42}, "chat": {"id": 42}}});
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
