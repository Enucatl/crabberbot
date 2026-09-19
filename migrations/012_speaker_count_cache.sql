-- Cache the speaker count returned alongside the diarized transcript and summary.
ALTER TABLE callback_contexts ADD COLUMN speaker_count INTEGER;
