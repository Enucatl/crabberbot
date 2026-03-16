pub mod audio_extractor;
pub mod summarizer;
pub mod transcriber;

/// Directory for extracted audio files awaiting transcription/sending.
pub const AUDIO_CACHE_DIR: &str = "/tmp/audio_cache";

/// Max per-file duration for AI features (transcription/summarization).
/// Prevents webhook timeouts, Deepgram choking on huge files, and RAM hogging.
pub const MAX_PREMIUM_FILE_DURATION_SECS: i32 = 1800; // 30 minutes

/// Per-second API cost in USD for Deepgram (for cost tracking in premium_usage table).
pub const DEEPGRAM_COST_PER_SECOND: f64 = 0.00013; // Deepgram Nova-3 ($0.0078/min)

/// Gemini API costs in USD per million tokens.
pub const GEMINI_INPUT_COST_PER_MILLION_TOKENS: f64 = 0.25;
pub const GEMINI_OUTPUT_COST_PER_MILLION_TOKENS: f64 = 1.50;
