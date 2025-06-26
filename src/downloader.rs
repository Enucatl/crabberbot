use async_trait::async_trait;
use mockall::automock;
use serde::Deserialize;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum DownloadError {
    #[error("yt-dlp command failed: {0}")]
    CommandFailed(String),
    #[error("Failed to parse yt-dlp output: {0}")]
    ParsingFailed(String),
    #[error("Could not create temporary directory: {0}")]
    IoError(String),
}

// New struct to hold parsed metadata for each media item
#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct MediaMetadata {
    #[serde(rename = "_filename")]
    pub filepath: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub title: String,
    pub ext: String,
    #[serde(rename = "_type", default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[automock]
#[async_trait]
pub trait Downloader {
    async fn download_media(
        &self,
        url: &str,
    ) -> Result<(String, Vec<MediaMetadata>), DownloadError>;
}

pub struct YtDlpDownloader;

#[async_trait]
impl Downloader for YtDlpDownloader {
    async fn download_media(
        &self,
        url: &str,
    ) -> Result<(String, Vec<MediaMetadata>), DownloadError> {
        let uuid = uuid::Uuid::new_v4().to_string();
        let filename_template = format!("{}.%(id)s.%(ext)s", uuid);

        log::info!("Downloading {}", url);

        let output = tokio::process::Command::new("yt-dlp")
            .arg("--print-json")
            .arg("--no-warnings")
            .arg("--ignore-config")
            .arg("-o")
            .arg(&filename_template)
            .arg(url)
            .output()
            .await
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("yt-dlp failed for url {}: {}", url, stderr);
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let mut downloaded_media_items = Vec::new();
        let mut final_caption_untruncated = String::new();

        for line in stdout_str.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MediaMetadata>(line) {
                Ok(metadata) => {
                    log::info!("Successfully downloaded and parsed: {}", metadata.filepath);
                    if final_caption_untruncated.is_empty() {
                        // This is the first item. Build the caption from its metadata.
                        let source_link = url;
                        let via_link = "https://t.me/crabberbot?start=c"; // As requested
                        let header = format!(
                            "<a href=\"{}\">Source</a> ✤ <a href=\"{}\">Via</a>",
                            source_link, via_link
                        );

                        let mut quote_parts = Vec::new();
                        if let Some(uploader) = &metadata.uploader {
                            if !uploader.is_empty() {
                                quote_parts.push(format!("@{}", uploader));
                            }
                        }

                        let description = metadata.description.trim();
                        if !description.is_empty() {
                            quote_parts.push(description.to_string());
                        }

                        final_caption_untruncated = if !quote_parts.is_empty() {
                            format!(
                                "{}\n\n<blockquote>{}</blockquote>",
                                header,
                                quote_parts.join("\n")
                            )
                        } else {
                            header
                        };
                    }
                    downloaded_media_items.push(metadata);
                }
                Err(e) => {
                    log::warn!("Failed to parse a line of yt-dlp JSON output: {}", e);
                }
            }
        }

        if downloaded_media_items.is_empty() {
            return Err(DownloadError::ParsingFailed(
                "Could not extract any media metadata from yt-dlp output.".to_string(),
            ));
        }

        let final_caption: String = final_caption_untruncated.chars().take(1024).collect();

        Ok((final_caption, downloaded_media_items))
    }
}
