use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SummarizationError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("Gemini API error: {0}")]
    ApiError(String),
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, transcript: &str) -> Result<String, SummarizationError>;
}

pub struct GeminiSummarizer {
    client: reqwest::Client,
    api_key: String,
}

impl GeminiSummarizer {
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

#[async_trait]
impl Summarizer for GeminiSummarizer {
    async fn summarize(&self, transcript: &str) -> Result<String, SummarizationError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={}",
            self.api_key
        );

        let body = serde_json::json!({
            "contents": [{
                "parts": [{
                    "text": format!(
                        "You are a helpful assistant that summarizes video content. \
                         Provide a concise summary (3-5 bullet points) of the following transcript:\n\n{}",
                        transcript
                    )
                }]
            }],
            "generationConfig": {
                "maxOutputTokens": 1024
            }
        });

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SummarizationError::ApiError(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let json: serde_json::Value = response.json().await?;

        let summary = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if summary.is_empty() {
            return Err(SummarizationError::ApiError(
                "Empty summary returned from Gemini".to_string(),
            ));
        }

        Ok(summary)
    }
}
