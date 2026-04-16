use async_trait::async_trait;
use reqwest::StatusCode;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::premium::{GEMINI_INPUT_COST_PER_MILLION_TOKENS, GEMINI_OUTPUT_COST_PER_MILLION_TOKENS};
use crate::retry::{
    RetryPolicy, retry_after_from_response, retry_async, retryable_status, transient_reqwest_error,
};

#[derive(Debug, Error)]
pub enum SummarizationError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("Gemini API error: {0}")]
    ApiError(String),
    #[error("Gemini transient API error: HTTP {status}")]
    RetryableApi {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
}

pub struct GeminiResult {
    pub text: String,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<GeminiResult, SummarizationError>;
    async fn correct_transcript(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<GeminiResult, SummarizationError>;
}

pub struct GeminiSummarizer {
    client: reqwest::Client,
    api_key: String,
    model: String,
    retry_policy: RetryPolicy,
    cooldown_until: Arc<Mutex<Option<std::time::Instant>>>,
}

impl GeminiSummarizer {
    pub fn new(client: reqwest::Client, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
            retry_policy: RetryPolicy::provider_default(),
            cooldown_until: Arc::new(Mutex::new(None)),
        }
    }

    async fn enforce_cooldown(&self) -> Result<(), SummarizationError> {
        let until = *self.cooldown_until.lock().await;
        if let Some(until) = until {
            let now = std::time::Instant::now();
            if until > now {
                return Err(SummarizationError::ApiError(format!(
                    "Gemini temporarily unavailable; retry after {:?}",
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

    /// Returns `(text, prompt_tokens, output_tokens)`.
    async fn call_gemini(&self, prompt: &str) -> Result<(String, u64, u64), SummarizationError> {
        self.enforce_cooldown().await?;
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let body = serde_json::json!({
            "contents": [{"parts": [{"text": prompt}]}],
            "generationConfig": {"temperature": 0.7, "maxOutputTokens": 8192, "thinkingConfig": {"thinkingLevel": "minimal"}},
            "safetySettings": [
                {"category": "HARM_CATEGORY_HARASSMENT",        "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_HATE_SPEECH",       "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE"}
            ]
        });

        log::debug!("Gemini request body: {}", body);

        let response_result = retry_async(
            &self.retry_policy,
            || async {
                let response = self
                    .client
                    .post(&url)
                    .timeout(Duration::from_secs(45))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await?;
                let status = response.status();
                if retryable_status(status) {
                    return Err(SummarizationError::RetryableApi {
                        status,
                        retry_after: retry_after_from_response(&response),
                    });
                }
                Ok(response)
            },
            |error| match error {
                SummarizationError::RetryableApi { retry_after, .. } => *retry_after,
                _ => None,
            },
            |error| match error {
                SummarizationError::HttpError(e) => transient_reqwest_error(e),
                SummarizationError::RetryableApi { .. } => true,
                _ => false,
            },
            "gemini.generate_content",
        )
        .await;
        let response = match response_result {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, SummarizationError::RetryableApi { .. }) {
                    log::error!("Gemini retry budget exhausted: {}", error);
                    self.start_cooldown().await;
                }
                return Err(error);
            }
        };

        let status = response.status();
        let text = response.text().await?;
        log::debug!("Gemini response status={} body={}", status, text);

        if !status.is_success() {
            return Err(SummarizationError::ApiError(format!(
                "HTTP {}: {}",
                status, text
            )));
        }

        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| SummarizationError::ApiError(format!("JSON parse error: {}", e)))?;

        let result = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if result.is_empty() {
            return Err(SummarizationError::ApiError(
                "Empty response returned from Gemini".to_string(),
            ));
        }

        let prompt_tokens = json["usageMetadata"]["promptTokenCount"]
            .as_u64()
            .unwrap_or(0);
        let output_tokens = json["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(0);

        Ok((result, prompt_tokens, output_tokens))
    }
}

#[async_trait]
impl Summarizer for GeminiSummarizer {
    async fn summarize(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<GeminiResult, SummarizationError> {
        let language_hint = match &language {
            Some(lang) => format!(
                "The two-letter code for the language is: {}. Answer only in that language.\n\n",
                lang
            ),
            None => String::new(),
        };
        let prompt = format!(
            "You are a helpful assistant that summarizes video content.\n\
             Provide a concise summary of the following transcript as 3 to 5 bullet points.\n\n\
             {}Each bullet point must be very short (one sentence at most).\n\
             Use the bullet character • for each point.\n\
             Separate each bullet point with a blank line.\n\
             Output only the bullet points, no preamble or closing remarks.\n\n\
             Transcript:\n\n{}",
            language_hint, transcript
        );
        log::info!(
            "Gemini summarize: transcript {} chars, language={:?}",
            transcript.len(),
            language
        );
        let (text, prompt_tokens, output_tokens) = self.call_gemini(&prompt).await?;
        let cost = prompt_tokens as f64 / 1_000_000.0 * GEMINI_INPUT_COST_PER_MILLION_TOKENS
            + output_tokens as f64 / 1_000_000.0 * GEMINI_OUTPUT_COST_PER_MILLION_TOKENS;
        log::info!(
            "Gemini summarize: tokens in={} out={} cost=${:.6}",
            prompt_tokens,
            output_tokens,
            cost
        );
        Ok(GeminiResult {
            text,
            prompt_tokens,
            output_tokens,
        })
    }

    async fn correct_transcript(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<GeminiResult, SummarizationError> {
        let language_hint = match &language {
            Some(lang) => format!(
                "The two-letter code for the language is: {}.\nKeep this in mind and answer only in the same language.\n\n",
                lang
            ),
            None => String::new(),
        };
        let prompt = format!(
            "I will copy the raw transcription of an audio, transcribed by AI.\n\
             Please review it for errors in spelling, punctuation, possibly mistranscribed words.\n\n\
             {}\
             Correct any mistakes you find, by staying as close as possible to the original phrasing.\n\
             Provide only the corrected version of the transcript, without any additional commentary, \
             preamble, or conversational phrases.\n\n\
             Add paragraphs by separating with an empty line to facilitate reading and comprehension.\n\
             Avoid the block of text feeling that you get from an overly long text with no breaks.\n\n\
             Original Transcript:\n\
             ---\n\
             {}\n\
             ---\n\
             Corrected Transcript:",
            language_hint, transcript
        );
        log::info!(
            "Gemini correction: transcript {} chars, language={:?}",
            transcript.len(),
            language
        );
        let (text, prompt_tokens, output_tokens) = self.call_gemini(&prompt).await?;
        let cost = prompt_tokens as f64 / 1_000_000.0 * GEMINI_INPUT_COST_PER_MILLION_TOKENS
            + output_tokens as f64 / 1_000_000.0 * GEMINI_OUTPUT_COST_PER_MILLION_TOKENS;
        log::info!(
            "Gemini correction: tokens in={} out={} cost=${:.6}",
            prompt_tokens,
            output_tokens,
            cost
        );
        Ok(GeminiResult {
            text: text.trim().to_string(),
            prompt_tokens,
            output_tokens,
        })
    }
}
