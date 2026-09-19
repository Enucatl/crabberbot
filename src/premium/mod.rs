pub mod audio_extractor;
pub mod summarizer;
pub mod transcriber;

/// Default audio cache directory path.
pub const DEFAULT_AUDIO_CACHE_DIR: &str = "/downloads/audio_cache";

/// Max per-file duration for AI features (transcription/summarization).
/// Prevents webhook timeouts, Deepgram choking on huge files, and RAM hogging.
pub const MAX_PREMIUM_FILE_DURATION_SECS: i32 = 1800; // 30 minutes

/// Per-second API cost in USD for Deepgram (for cost tracking in premium_usage table).
pub const DEEPGRAM_COST_PER_SECOND: f64 = 0.00013; // Deepgram Nova-3 ($0.0078/min)
