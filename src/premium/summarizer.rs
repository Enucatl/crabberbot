use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::StatusCode;
use std::time::Duration;
use thiserror::Error;

use crate::retry::{
    ProviderCooldown, RetryPolicy, retry_after_from_response, retry_async, retryable_status,
    transient_reqwest_error,
};

#[derive(Debug, Error)]
pub enum SummarizationError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("OpenRouter API error: {0}")]
    ApiError(String),
    #[error("OpenRouter transient API error: HTTP {status}")]
    RetryableApi {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
}

pub struct OpenRouterResult {
    pub text: String,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<OpenRouterResult, SummarizationError>;
    async fn correct_transcript(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<OpenRouterResult, SummarizationError>;
}

pub struct OpenRouterSummarizer {
    client: reqwest::Client,
    api_key: String,
    model: String,
    retry_policy: RetryPolicy,
    cooldown: ProviderCooldown,
}

impl OpenRouterSummarizer {
    pub fn new(client: reqwest::Client, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
            retry_policy: RetryPolicy::provider_default(),
            cooldown: ProviderCooldown::new("OpenRouter"),
        }
    }

    fn chat_completions_url(&self) -> &'static str {
        "https://openrouter.ai/api/v1/chat/completions"
    }

    fn chat_completions_request(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .timeout(Duration::from_secs(45))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(body)
    }

    /// Returns `(text, prompt_tokens, output_tokens, cost_usd)`.
    async fn call_openrouter(
        &self,
        prompt: &str,
    ) -> Result<(String, u64, u64, f64), SummarizationError> {
        self.cooldown
            .check()
            .await
            .map_err(SummarizationError::ApiError)?;
        let url = self.chat_completions_url();

        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.7,
            "max_tokens": 8192,
            "usage": {"include": true}
        });

        log::debug!("OpenRouter request body: {}", body);

        let response_result = retry_async(
            &self.retry_policy,
            || async {
                let response = self.chat_completions_request(url, &body).send().await?;
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
            "openrouter.chat_completions",
        )
        .await;
        let response = match response_result {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, SummarizationError::RetryableApi { .. }) {
                    log::error!("OpenRouter retry budget exhausted: {}", error);
                    self.cooldown.start(Duration::from_secs(30)).await;
                }
                return Err(error);
            }
        };

        let status = response.status();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(SummarizationError::ApiError(
                    "response exceeded 1 MiB".to_string(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        log::debug!("OpenRouter response status={} body={}", status, text);

        if !status.is_success() {
            return Err(SummarizationError::ApiError(format!(
                "HTTP {}: {}",
                status, text
            )));
        }

        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| SummarizationError::ApiError(format!("JSON parse error: {}", e)))?;

        let result = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if result.is_empty() {
            return Err(SummarizationError::ApiError(
                "Empty response returned from OpenRouter".to_string(),
            ));
        }

        let prompt_tokens = json["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let output_tokens = json["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        let cost_usd = json["usage"]["cost"].as_f64().unwrap_or(0.0);

        Ok((result, prompt_tokens, output_tokens, cost_usd))
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    const SENTINEL_API_KEY: &str = "sentinel-openrouter-api-key";

    #[test]
    fn openrouter_request_sends_api_key_in_header() {
        let summarizer = OpenRouterSummarizer::new(
            reqwest::Client::new(),
            SENTINEL_API_KEY.to_string(),
            "test-model".to_string(),
        );
        let url = summarizer.chat_completions_url();
        let request = summarizer
            .chat_completions_request(url, &serde_json::json!({}))
            .build()
            .unwrap();

        assert!(request.url().query_pairs().all(|(name, _)| name != "key"));
        assert!(!request.url().as_str().contains(SENTINEL_API_KEY));
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {SENTINEL_API_KEY}")
        );
    }

    #[tokio::test]
    async fn openrouter_connection_error_does_not_expose_api_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::builder()
            .resolve("openrouter.ai", address)
            .build()
            .unwrap();
        let mut summarizer = OpenRouterSummarizer::new(
            client,
            SENTINEL_API_KEY.to_string(),
            "test-model".to_string(),
        );
        summarizer.retry_policy.max_attempts = 1;

        let error = summarizer.call_openrouter("test").await.unwrap_err();

        assert!(!error.to_string().contains(SENTINEL_API_KEY));
    }
}

#[allow(clippy::items_after_test_module)]
#[async_trait]
impl Summarizer for OpenRouterSummarizer {
    async fn summarize(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<OpenRouterResult, SummarizationError> {
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
            "OpenRouter summarize: transcript {} chars, language={:?}",
            transcript.len(),
            language
        );
        let (text, prompt_tokens, output_tokens, cost_usd) = self.call_openrouter(&prompt).await?;
        log::info!(
            "OpenRouter summarize: tokens in={} out={} cost=${:.6}",
            prompt_tokens,
            output_tokens,
            cost_usd
        );
        Ok(OpenRouterResult {
            text,
            prompt_tokens,
            output_tokens,
            cost_usd,
        })
    }

    async fn correct_transcript(
        &self,
        transcript: &str,
        language: Option<String>,
    ) -> Result<OpenRouterResult, SummarizationError> {
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
            "OpenRouter correction: transcript {} chars, language={:?}",
            transcript.len(),
            language
        );
        let (text, prompt_tokens, output_tokens, cost_usd) = self.call_openrouter(&prompt).await?;
        log::info!(
            "OpenRouter correction: tokens in={} out={} cost=${:.6}",
            prompt_tokens,
            output_tokens,
            cost_usd
        );
        Ok(OpenRouterResult {
            text: text.trim().to_string(),
            prompt_tokens,
            output_tokens,
            cost_usd,
        })
    }
}
