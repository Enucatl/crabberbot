ALTER TABLE subscriptions
    ADD CONSTRAINT subscriptions_tier_valid CHECK (tier IN ('free', 'basic', 'pro', 'ultra')),
    ADD CONSTRAINT subscriptions_quota_nonnegative CHECK (
        ai_seconds_used >= 0 AND ai_seconds_limit >= 0 AND topup_seconds_available >= 0
    );

ALTER TABLE payments
    ADD CONSTRAINT payments_product_valid CHECK (product IN ('sub_basic', 'sub_pro', 'topup_60')),
    ADD CONSTRAINT payments_amount_positive CHECK (amount > 0);

ALTER TABLE premium_usage
    ADD CONSTRAINT premium_usage_duration_nonnegative CHECK (duration_secs >= 0),
    ADD CONSTRAINT premium_usage_cost_nonnegative CHECK (estimated_cost_usd >= 0),
    ADD CONSTRAINT premium_usage_units_nonnegative CHECK (units >= 0);
