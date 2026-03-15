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
    async fn summarize(&self, transcript: &str, language: Option<String>) -> Result<String, SummarizationError>;
    async fn correct_transcript(&self, transcript: &str, language: Option<String>) -> Result<String, SummarizationError>;
}

pub struct GeminiSummarizer {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiSummarizer {
    pub fn new(client: reqwest::Client, api_key: String, model: String) -> Self {
        Self { client, api_key, model }
    }

    async fn call_gemini(&self, prompt: &str) -> Result<String, SummarizationError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let body = serde_json::json!({
            "contents": [{"parts": [{"text": prompt}]}],
            "generationConfig": {"temperature": 0.7, "maxOutputTokens": 8192},
            "safetySettings": [
                {"category": "HARM_CATEGORY_HARASSMENT",        "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_HATE_SPEECH",       "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE"},
                {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE"}
            ]
        });

        log::debug!("Gemini request body: {}", body);

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

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

        Ok(result)
    }
}

#[async_trait]
impl Summarizer for GeminiSummarizer {
    async fn summarize(&self, transcript: &str, language: Option<String>) -> Result<String, SummarizationError> {
        let language_hint = match &language {
            Some(lang) => format!("The two-letter code for the language is: {}. Answer only in that language.\n\n", lang),
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
        log::info!("Gemini summarize: transcript {} chars, language={:?}", transcript.len(), language);
        self.call_gemini(&prompt).await
    }

    async fn correct_transcript(&self, transcript: &str, language: Option<String>) -> Result<String, SummarizationError> {
        let language_hint = match &language {
            Some(lang) => format!("The two-letter code for the language is: {}.\nKeep this in mind and answer only in the same language.\n\n", lang),
            None => String::new(),
        };
        let prompt = format!(
            "I will copy the raw transcription of an audio, transcribed by AI.\n\
             Please review it for errors in spelling, punctuation, possibly mistranscribed words.\n\n\
             Add paragraphs by separating with an empty line to facilitate reading and comprehension.\n\n\
             {}\
             Correct any mistakes you find, by staying as close as possible to the original phrasing.\n\
             Provide only the corrected version of the transcript, without any additional commentary, \
             preamble, or conversational phrases.\n\n\
             Original Transcript:\n\
             ---\n\
             {}\n\
             ---\n\
             Corrected Transcript:",
            language_hint,
            transcript
        );
        log::info!("Gemini correction: transcript {} chars, language={:?}", transcript.len(), language);
        self.call_gemini(&prompt).await.map(|s| s.trim().to_string())
    }
}
