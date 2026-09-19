-- Keep the original Deepgram/diarized input separately from the cleaned transcript.
ALTER TABLE callback_contexts ADD COLUMN raw_transcript TEXT;
