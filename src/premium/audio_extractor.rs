use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::worker_protocol::{Request, Response, ResultData, read_frame, write_frame};

#[derive(Debug, Error)]
pub enum AudioExtractionError {
    #[error("audio tool timed out after {0} seconds")]
    Timeout(u64),
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
        title: Option<String>,
        author: Option<String>,
    ) -> Result<AudioExtractionResult, AudioExtractionError>;
}

pub struct SocketAudioExtractor {
    socket_path: PathBuf,
}

impl SocketAudioExtractor {
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }
}

#[async_trait]
impl AudioExtractor for SocketAudioExtractor {
    async fn extract_audio(
        &self,
        video_path: &Path,
        title: Option<String>,
        author: Option<String>,
    ) -> Result<AudioExtractionResult, AudioExtractionError> {
        let mut stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| {
                AudioExtractionError::FfmpegError(format!("downloader worker unavailable: {error}"))
            })?;
        write_frame(
            &mut stream,
            &Request::ExtractAudio {
                video_path: video_path.to_string_lossy().into_owned(),
                title,
                author,
            },
        )
        .await
        .map_err(AudioExtractionError::FfmpegError)?;
        let frame = read_frame(&mut stream)
            .await
            .map_err(AudioExtractionError::FfmpegError)?;
        match serde_json::from_slice::<Response>(&frame)
            .map_err(|error| AudioExtractionError::ParseError(error.to_string()))?
        {
            Response::Ok {
                result:
                    ResultData::Audio {
                        audio_path,
                        duration_secs,
                    },
            } => Ok(AudioExtractionResult {
                audio_path: PathBuf::from(audio_path),
                duration_secs,
            }),
            Response::Error { kind, message } if kind == "timeout" => Err(message
                .parse()
                .map(AudioExtractionError::Timeout)
                .unwrap_or(AudioExtractionError::FfmpegError(message))),
            Response::Error { kind, message } if kind == "parse" => {
                Err(AudioExtractionError::ParseError(message))
            }
            Response::Error { message, .. } => Err(AudioExtractionError::FfmpegError(message)),
            _ => Err(AudioExtractionError::ParseError(
                "unexpected downloader response".to_string(),
            )),
        }
    }
}

pub struct FfmpegAudioExtractor {
    semaphore: Arc<Semaphore>,
    audio_cache_dir: PathBuf,
}

impl FfmpegAudioExtractor {
    pub fn new(permits: usize, audio_cache_dir: PathBuf) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            audio_cache_dir,
        }
    }
}

#[async_trait]
impl AudioExtractor for FfmpegAudioExtractor {
    async fn extract_audio(
        &self,
        video_path: &Path,
        title: Option<String>,
        author: Option<String>,
    ) -> Result<AudioExtractionResult, AudioExtractionError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");

        // Step 1: ffprobe to get duration
        let mut ffprobe = tokio::process::Command::new("ffprobe");
        ffprobe.kill_on_drop(true);
        let ffprobe_output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ffprobe
                .args([
                    "-v",
                    "quiet",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "json",
                ])
                .arg(video_path)
                .output(),
        )
        .await
        .map_err(|_| AudioExtractionError::Timeout(30))?
        .map_err(|e| AudioExtractionError::FfprobeError(e.to_string()))?;

        if !ffprobe_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffprobe_output.stderr).to_string();
            return Err(AudioExtractionError::FfprobeError(stderr));
        }

        let ffprobe_json: serde_json::Value = serde_json::from_slice(&ffprobe_output.stdout)
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
        let audio_path = self.audio_cache_dir.join(&audio_filename);

        const MAX_TAG_LEN: usize = 255;
        let mut cmd = tokio::process::Command::new("ffmpeg");
        cmd.kill_on_drop(true);
        cmd.args(["-i"]).arg(video_path).args([
            "-vn",
            "-acodec",
            "libmp3lame",
            "-q:a",
            "2",
            "-threads",
            "1",
        ]);
        if let Some(t) = title {
            let truncated: String = t.chars().take(MAX_TAG_LEN).collect();
            cmd.args(["-metadata", &format!("title={truncated}")]);
        }
        if let Some(a) = author {
            let truncated: String = a.chars().take(MAX_TAG_LEN).collect();
            cmd.args(["-metadata", &format!("artist={truncated}")]);
        }
        cmd.args(["-y"]).arg(&audio_path);

        let ffmpeg_output =
            match tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output()).await {
                Ok(Ok(output)) => output,
                Ok(Err(e)) => {
                    let _ = tokio::fs::remove_file(&audio_path).await;
                    return Err(AudioExtractionError::FfmpegError(e.to_string()));
                }
                Err(_) => {
                    let _ = tokio::fs::remove_file(&audio_path).await;
                    return Err(AudioExtractionError::Timeout(300));
                }
            };

        if !ffmpeg_output.status.success() {
            let _ = tokio::fs::remove_file(&audio_path).await;
            let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr).to_string();
            return Err(AudioExtractionError::FfmpegError(stderr));
        }

        Ok(AudioExtractionResult {
            audio_path,
            duration_secs,
        })
    }
}
