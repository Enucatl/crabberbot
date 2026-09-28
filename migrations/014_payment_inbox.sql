CREATE TABLE payment_inbox (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('payment', 'refund')),
    telegram_charge_id TEXT NOT NULL,
    user_id BIGINT NOT NULL,
    provider_charge_id TEXT,
    product TEXT,
    amount INTEGER,
    processed_at TIMESTAMPTZ,
    UNIQUE (kind, telegram_charge_id)
);

CREATE INDEX payment_inbox_pending ON payment_inbox (id) WHERE processed_at IS NULL;
