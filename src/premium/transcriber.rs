use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptionError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("Deepgram API error: {0}")]
    ApiError(String),
    #[error("Failed to read audio file: {0}")]
    IoError(#[from] std::io::Error),
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio_path: &Path) -> Result<String, TranscriptionError>;
}

pub struct DeepgramTranscriber {
    client: reqwest::Client,
    api_key: String,
}

impl DeepgramTranscriber {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

#[async_trait]
impl Transcriber for DeepgramTranscriber {
    async fn transcribe(&self, audio_path: &Path) -> Result<String, TranscriptionError> {
        let audio_bytes = tokio::fs::read(audio_path).await?;

        let response = self
            .client
            .post("https://api.deepgram.com/v1/listen?model=nova-3&smart_format=true")
            .header("Authorization", format!("Token {}", self.api_key))
            .header("Content-Type", "audio/mpeg")
            .body(audio_bytes)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(TranscriptionError::ApiError(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let json: serde_json::Value = response.json().await?;

        let transcript = json["results"]["channels"][0]["alternatives"][0]["transcript"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if transcript.is_empty() {
            return Err(TranscriptionError::ApiError(
                "Empty transcript returned from Deepgram".to_string(),
            ));
        }

        Ok(transcript)
    }
}
