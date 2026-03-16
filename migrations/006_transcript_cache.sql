-- Cache the Deepgram transcript alongside the callback context so that
-- Transcribe and Summarize button clicks never call Deepgram twice for the
-- same video.  Both columns are NULL until the first transcription request.
ALTER TABLE callback_contexts ADD COLUMN transcript TEXT;
ALTER TABLE callback_contexts ADD COLUMN transcript_language TEXT;
