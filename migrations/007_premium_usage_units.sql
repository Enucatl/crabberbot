-- Add raw API-reported units to premium_usage for undisputable cost auditing.
-- Meaning of units depends on feature:
--   transcribe / summarize  → billed audio seconds (Deepgram metadata.duration)
--   gemini_*_input          → prompt token count
--   gemini_*_output         → candidates token count
--   audio_extract           → 0 (no API billing)
ALTER TABLE premium_usage ADD COLUMN units REAL NOT NULL DEFAULT 0;
