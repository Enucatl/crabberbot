pub mod audio_extractor;
pub mod summarizer;
pub mod transcriber;

/// Directory for extracted audio files awaiting transcription/sending.
pub const AUDIO_CACHE_DIR: &str = "/tmp/audio_cache";

/// Max per-file duration for AI features (transcription/summarization).
/// Prevents webhook timeouts, Deepgram choking on huge files, and RAM hogging.
pub const MAX_PREMIUM_FILE_DURATION_SECS: i32 = 1800; // 30 minutes

/// Per-second API costs in USD (for cost tracking in premium_usage table)
pub const DEEPGRAM_COST_PER_SECOND: f64 = 0.00013; // Deepgram Nova-3 ($0.0078/min)
pub const GEMINI_COST_PER_SECOND: f64 = 0.0000011; // Google Gemini (~$0.004/hr)
