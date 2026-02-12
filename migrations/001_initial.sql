CREATE TABLE media_cache (
    id SERIAL PRIMARY KEY,
    source_url TEXT UNIQUE NOT NULL,
    caption TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE cached_files (
    id SERIAL PRIMARY KEY,
    cache_id INTEGER NOT NULL REFERENCES media_cache(id) ON DELETE CASCADE,
    telegram_file_id TEXT NOT NULL,
    media_type TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_cached_files_cache_id ON cached_files(cache_id);

CREATE TABLE requests (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    source_url TEXT NOT NULL,
    status TEXT NOT NULL,
    processing_time_ms INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_requests_created_at ON requests(created_at);
