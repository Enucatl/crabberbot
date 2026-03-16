use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::premium::AUDIO_CACHE_DIR;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Semaphore;

#[derive(Debug, Error)]
pub enum AudioExtractionError {
    #[error("ffprobe failed: {0}")]
    FfprobeError(String),
    #[error("ffmpeg failed: {0}")]
    FfmpegError(String),
    #[error("Failed to parse ffprobe output: {0}")]
    ParseError(String),
}

pub struct AudioExtractionResult {
    pub audio_path: PathBuf,
    pub duration_secs: i32,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait AudioExtractor: Send + Sync {
    async fn extract_audio(
        &self,
        video_path: &Path,
        title: Option<&str>,
        author: Option<&str>,
    ) -> Result<AudioExtractionResult, AudioExtractionError>;
}

pub struct FfmpegAudioExtractor {
    semaphore: Arc<Semaphore>,
}

impl FfmpegAudioExtractor {
    pub fn new(permits: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
        }
    }
}

#[async_trait]
impl AudioExtractor for FfmpegAudioExtractor {
    async fn extract_audio(
        &self,
        video_path: &Path,
        title: Option<&str>,
        author: Option<&str>,
    ) -> Result<AudioExtractionResult, AudioExtractionError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        // Step 1: ffprobe to get duration
        let ffprobe_output = tokio::process::Command::new("ffprobe")
            .args([
                "-v", "quiet",
                "-show_entries", "format=duration",
                "-of", "json",
            ])
            .arg(video_path)
            .output()
            .await
            .map_err(|e| AudioExtractionError::FfprobeError(e.to_string()))?;

        if !ffprobe_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffprobe_output.stderr).to_string();
            return Err(AudioExtractionError::FfprobeError(stderr));
        }

        let ffprobe_json: serde_json::Value =
            serde_json::from_slice(&ffprobe_output.stdout)
                .map_err(|e| AudioExtractionError::ParseError(e.to_string()))?;

        let duration_secs = ffprobe_json["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|d| d.round() as i32)
            .ok_or_else(|| {
                AudioExtractionError::ParseError("missing duration in ffprobe output".to_string())
            })?;

        // Step 2: ffmpeg to extract audio
        let audio_filename = format!("{}.mp3", uuid::Uuid::new_v4());
        let audio_path = PathBuf::from(AUDIO_CACHE_DIR).join(&audio_filename);

        const MAX_TAG_LEN: usize = 255;
        let mut cmd = tokio::process::Command::new("ffmpeg");
        cmd.args(["-i"]).arg(video_path).args([
            "-vn",
            "-acodec", "libmp3lame",
            "-q:a", "2",
            "-threads", "1",
        ]);
        if let Some(t) = title {
            let truncated: String = t.chars().take(MAX_TAG_LEN).collect();
            cmd.args(["-metadata", &format!("title={}", truncated)]);
        }
        if let Some(a) = author {
            let truncated: String = a.chars().take(MAX_TAG_LEN).collect();
            cmd.args(["-metadata", &format!("artist={}", truncated)]);
        }
        cmd.args(["-y"]).arg(&audio_path);

        let ffmpeg_output = cmd
            .output()
            .await
            .map_err(|e| AudioExtractionError::FfmpegError(e.to_string()))?;

        if !ffmpeg_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr).to_string();
            return Err(AudioExtractionError::FfmpegError(stderr));
        }

        Ok(AudioExtractionResult {
            audio_path,
            duration_secs,
        })
    }
}
