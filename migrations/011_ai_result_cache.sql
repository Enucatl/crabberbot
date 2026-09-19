-- Cache the structured OpenRouter result so Transcribe and Summarize share one model call.
ALTER TABLE callback_contexts ADD COLUMN summary TEXT;
