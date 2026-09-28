ALTER TABLE payments ADD COLUMN telegram_update_id BIGINT NOT NULL DEFAULT 0;
ALTER TABLE payment_inbox ADD COLUMN paid_at TIMESTAMPTZ;
ALTER TABLE payment_inbox ADD COLUMN telegram_update_id BIGINT NOT NULL DEFAULT 0;
-- Preserve intake order when Telegram's original sequence was not retained.
UPDATE payment_inbox SET telegram_update_id = id;

UPDATE payment_inbox i SET paid_at = p.created_at
FROM payments p WHERE p.telegram_payment_charge_id = i.telegram_charge_id;
-- Older inbox rows did not retain Telegram's date. Treat unknown events as
-- oldest so recovering one cannot overwrite an existing paid tier or grant.
UPDATE payment_inbox SET paid_at = '1970-01-01 UTC' WHERE paid_at IS NULL;
ALTER TABLE payment_inbox ALTER COLUMN paid_at SET NOT NULL;

-- Keep the order even after a refund clears source_payment_id, so replaying
-- an older purchase cannot resurrect an entitlement after a newer refund.
ALTER TABLE subscriptions ADD COLUMN entitlement_at TIMESTAMPTZ NOT NULL DEFAULT '-infinity';
ALTER TABLE subscriptions ADD COLUMN entitlement_update_id BIGINT NOT NULL DEFAULT 0;
UPDATE subscriptions s SET entitlement_at = COALESCE(
    (SELECT p.created_at FROM payments p WHERE p.id = s.source_payment_id),
    s.updated_at
)
WHERE s.source_payment_id IS NOT NULL OR s.tier <> 'free' OR s.expires_at IS NOT NULL
   OR EXISTS (SELECT 1 FROM payments p WHERE p.user_id = s.user_id
              AND p.product IN ('sub_basic', 'sub_pro'));
