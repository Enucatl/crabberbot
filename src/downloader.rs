use async_trait::async_trait;
use mockall::automock;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

#[derive(Error, Debug, PartialEq)]
pub enum DownloadError {
    #[error("yt-dlp command failed: {0}")]
    CommandFailed(String),
    #[error("Failed to parse yt-dlp output: {0}")]
    ParsingFailed(String),
    #[error("Could not create temporary directory: {0}")]
    IoError(String),
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct MediaMetadata {
    // ---- Fields for Post-Download Info
    #[serde(rename = "_filename")]
    pub filepath: Option<String>,
    pub ext: Option<String>,

    // ---- Fields for Both Pre- and Post-Download Info ----
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "_type", default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub uploader: Option<String>,

    // Duration of the video in seconds.
    #[serde(default)]
    pub duration: Option<f64>,

    // Approximate file size in bytes. yt-dlp often provides this.
    // We use `filesize_approx` as that's a common field name in its JSON output.
    #[serde(rename = "filesize_approx", default)]
    pub filesize: Option<u64>,

    // If the URL is a playlist, this field will contain a list of metadata
    // for each item in the playlist. This is how we count them.
    #[serde(default)]
    pub entries: Option<Vec<MediaMetadata>>,

    // -- Unused but useful for debugging
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,

    // We use `#[serde(skip)]` because this field is not part of yt-dlp's JSON output.
    // We will populate it ourselves after the download.
    #[serde(skip)]
    pub final_caption: String,
}

#[automock]
#[async_trait]
pub trait Downloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaMetadata, DownloadError>;
    async fn download_media(&self, url: &Url) -> Result<MediaMetadata, DownloadError>;
}

pub struct YtDlpDownloader;

#[async_trait]
impl Downloader for YtDlpDownloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaMetadata, DownloadError> {
        log::info!("Fetching metadata for {}", url);

        let output = tokio::process::Command::new("yt-dlp")
            .arg("--dump-single-json")
            .arg("--no-warnings")
            .arg("--ignore-config")
            .arg(url.as_str())
            .output()
            .await
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!(
                "yt-dlp --dump-single-json failed for url {}: {}",
                url,
                stderr
            );
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);

        serde_json::from_str::<MediaMetadata>(&stdout_str).map_err(|e| {
            log::error!("Failed to parse metadata JSON for {}: {}", url, e);
            DownloadError::ParsingFailed(e.to_string())
        })
    }

    async fn download_media(&self, url: &Url) -> Result<MediaMetadata, DownloadError> {
        let uuid = uuid::Uuid::new_v4().to_string();
        let filename_template = format!("{}.%(id)s.%(ext)s", uuid);

        log::info!("Downloading {}", url);

        let output = tokio::process::Command::new("yt-dlp")
            .arg("--print-json")
            .arg("--no-warnings")
            .arg("--ignore-config")
            .arg("-o")
            .arg(&filename_template)
            .arg(url.as_str())
            .output()
            .await
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("yt-dlp failed for url {}: {}", url, stderr);
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        // This will hold the metadata for each individual file downloaded.
        let mut downloaded_items = Vec::new();

        for line in stdout_str.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MediaMetadata>(line) {
                Ok(metadata) => {
                    let path_str = metadata.filepath.as_deref().unwrap_or("[unknown filepath]");
                    log::info!("Successfully downloaded and parsed: {}", path_str);
                    downloaded_items.push(metadata);
                }
                Err(e) => {
                    log::warn!("Failed to parse a line of yt-dlp JSON output: {}", e);
                }
            }
        }

        if downloaded_items.is_empty() {
            return Err(DownloadError::ParsingFailed(
                "Could not extract any media metadata from yt-dlp output.".to_string(),
            ));
        }

        // Now, we construct the single, final MediaMetadata object.
        // We'll use the metadata from the *first* downloaded item to build the caption
        // and serve as the base for our return object.
        let mut result_metadata = downloaded_items[0].clone();

        // Build the caption
        let source_link = url;
        let via_link = "https://t.me/crabberbot?start=c";
        let header = format!(
            "<a href=\"{}\">CrabberBot</a> 🦀 <a href=\"{}\">Source</a>",
            via_link, source_link
        );

        let mut quote_parts = Vec::new();
        if let Some(uploader) = &result_metadata.uploader {
            if !uploader.is_empty() {
                quote_parts.push(format!("<i>{}</i>", uploader));
            }
        }
        if let Some(description) = &result_metadata.description {
            let description = description.trim();
            if !description.is_empty() {
                quote_parts.push(description.to_string());
            }
        }

        let final_caption_untruncated = if !quote_parts.is_empty() {
            format!(
                "{}\n\n<blockquote>{}</blockquote>",
                header,
                quote_parts.join("\n")
            )
        } else {
            header
        };

        // Truncate and store the caption in our new field.
        result_metadata.final_caption = final_caption_untruncated.chars().take(1024).collect();

        // If there was more than one item, populate the 'entries' field.
        if downloaded_items.len() > 1 {
            result_metadata.entries = Some(downloaded_items);
        }

        Ok(result_metadata)
    }
}
