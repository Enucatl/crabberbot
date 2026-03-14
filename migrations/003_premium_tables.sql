CREATE TABLE subscriptions (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL UNIQUE,
    tier TEXT NOT NULL DEFAULT 'free',
    ai_seconds_used INTEGER NOT NULL DEFAULT 0,
    ai_seconds_limit INTEGER NOT NULL DEFAULT 0,
    topup_seconds_available INTEGER NOT NULL DEFAULT 0,
    last_topup_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE payments (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    telegram_payment_charge_id TEXT NOT NULL UNIQUE,
    provider_payment_charge_id TEXT NOT NULL,
    product TEXT NOT NULL,
    amount INTEGER NOT NULL,
    currency TEXT NOT NULL DEFAULT 'XTR',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE premium_usage (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    feature TEXT NOT NULL,
    source_url TEXT NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0.0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE callback_contexts (
    id SERIAL PRIMARY KEY,
    source_url TEXT NOT NULL,
    chat_id BIGINT NOT NULL,
    has_video BOOLEAN NOT NULL DEFAULT TRUE,
    media_duration_secs INTEGER,
    audio_cache_path TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_subscriptions_chat_id ON subscriptions(chat_id);
CREATE INDEX idx_callback_contexts_created ON callback_contexts(created_at);
