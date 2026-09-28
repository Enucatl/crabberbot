ALTER TABLE subscriptions ADD COLUMN source_payment_id INTEGER REFERENCES payments(id);

-- Existing paid tiers have the exact expiry written in their payment transaction.
-- Leave other rows unlinked: an owner grant must survive an old charge's refund.
UPDATE subscriptions s SET source_payment_id = p.id
FROM payments p
WHERE p.user_id = s.user_id
  AND p.refunded_at IS NULL
  AND s.expires_at = p.created_at + INTERVAL '30 days'
  AND ((s.tier = 'basic' AND p.product = 'sub_basic')
    OR (s.tier = 'pro' AND p.product = 'sub_pro'));
