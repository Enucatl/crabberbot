-- Persist the extracted audio file path and video duration alongside the cached media.
-- This allows premium buttons (Extract Audio, Transcribe, Summarize) to be offered
-- on cache hits without re-downloading from the source. The audio file lives until
-- the cache entry is expired by cleanup_expired.

ALTER TABLE media_cache ADD COLUMN audio_cache_path TEXT;
ALTER TABLE media_cache ADD COLUMN media_duration_secs INTEGER;
