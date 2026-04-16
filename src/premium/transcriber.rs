use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::premium::DEEPGRAM_COST_PER_SECOND;
use crate::retry::{
    RetryPolicy, retry_after_from_response, retry_async, retryable_status, transient_reqwest_error,
};

#[derive(Debug, Error)]
pub enum TranscriptionError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("Deepgram API error: {0}")]
    ApiError(String),
    #[error("Deepgram transient API error: HTTP {status}")]
    RetryableApi {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    #[error("Failed to read audio file: {0}")]
    IoError(#[from] std::io::Error),
}

pub struct TranscriptionResult {
    pub transcript: String,
    /// BCP-47 language code detected by Deepgram (e.g. "it", "en"), if available.
    pub detected_language: Option<String>,
    /// Audio duration as reported by Deepgram (what they bill on).
    pub billed_duration_secs: f64,
    /// Estimated USD cost based on billed duration.
    pub cost_usd: f64,
}

/// Billing information from a Deepgram call, used to record usage and deduct quota.
pub struct DeepgramUsage {
    pub billed_duration_secs: f64,
    pub cost_usd: f64,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(
        &self,
        audio_path: &Path,
    ) -> Result<TranscriptionResult, TranscriptionError>;
}

pub struct DeepgramTranscriber {
    client: reqwest::Client,
    api_key: String,
    retry_policy: RetryPolicy,
    cooldown_until: Arc<Mutex<Option<std::time::Instant>>>,
}

impl DeepgramTranscriber {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            retry_policy: RetryPolicy::provider_default(),
            cooldown_until: Arc::new(Mutex::new(None)),
        }
    }

    async fn enforce_cooldown(&self) -> Result<(), TranscriptionError> {
        let until = *self.cooldown_until.lock().await;
        if let Some(until) = until {
            let now = std::time::Instant::now();
            if until > now {
                return Err(TranscriptionError::ApiError(format!(
                    "Deepgram temporarily unavailable; retry after {:?}",
                    until - now
                )));
            }
        }
        Ok(())
    }

    async fn start_cooldown(&self) {
        *self.cooldown_until.lock().await =
            Some(std::time::Instant::now() + Duration::from_secs(30));
    }
}

#[async_trait]
impl Transcriber for DeepgramTranscriber {
    async fn transcribe(
        &self,
        audio_path: &Path,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        self.enforce_cooldown().await?;
        let audio_bytes = tokio::fs::read(audio_path).await?;
        log::debug!(
            "Deepgram: sending {} bytes from {:?}",
            audio_bytes.len(),
            audio_path
        );

        let response_result = retry_async(
            &self.retry_policy,
            || async {
                let response = self
                    .client
                    .post("https://api.deepgram.com/v1/listen?model=nova-3&smart_format=true&detect_language=true")
                    .timeout(Duration::from_secs(45))
                    .header("Authorization", format!("Token {}", self.api_key))
                    .header("Content-Type", "audio/mpeg")
                    .body(audio_bytes.clone())
                    .send()
                    .await?;
                let status = response.status();
                if retryable_status(status) {
                    return Err(TranscriptionError::RetryableApi {
                        status,
                        retry_after: retry_after_from_response(&response),
                    });
                }
                Ok(response)
            },
            |error| match error {
                TranscriptionError::RetryableApi { retry_after, .. } => *retry_after,
                _ => None,
            },
            |error| match error {
                TranscriptionError::HttpError(e) => transient_reqwest_error(e),
                TranscriptionError::RetryableApi { .. } => true,
                _ => false,
            },
            "deepgram.transcribe",
        )
        .await;
        let response = match response_result {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, TranscriptionError::RetryableApi { .. }) {
                    log::error!("Deepgram retry budget exhausted: {}", error);
                    self.start_cooldown().await;
                }
                return Err(error);
            }
        };

        let status = response.status();
        let body = response.text().await?;
        log::debug!("Deepgram response status={} body={}", status, body);

        if !status.is_success() {
            return Err(TranscriptionError::ApiError(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| TranscriptionError::ApiError(format!("JSON parse error: {}", e)))?;

        let transcript = json["results"]["channels"][0]["alternatives"][0]["transcript"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let detected_language = json["results"]["channels"][0]["detected_language"]
            .as_str()
            .map(String::from);

        let billed_duration_secs = json["metadata"]["duration"].as_f64().unwrap_or(0.0);
        let cost_usd = billed_duration_secs * DEEPGRAM_COST_PER_SECOND;

        log::info!(
            "Deepgram: transcript {} chars, detected_language={:?}, duration={:.2}s, cost=${:.6}",
            transcript.len(),
            detected_language,
            billed_duration_secs,
            cost_usd,
        );

        if transcript.is_empty() {
            return Err(TranscriptionError::ApiError(
                "Empty transcript returned from Deepgram".to_string(),
            ));
        }

        Ok(TranscriptionResult {
            transcript,
            detected_language,
            billed_duration_secs,
            cost_usd,
        })
    }
}
