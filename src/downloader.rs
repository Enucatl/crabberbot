use std::collections::HashMap;

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
    pub id: String,
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
    #[serde(default)]
    pub playlist_uploader: Option<String>,

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

impl MediaMetadata {
    /// Determines the Telegram media type ("photo" or "video") based on extension.
    pub fn telegram_media_type(&self) -> Option<&'static str> {
        if let Some(ext) = &self.ext {
            match ext.as_str() {
                "mp4" | "webm" | "gif" | "mov" => Some("video"),
                "jpg" | "jpeg" | "png" | "webp" => Some("photo"),
                _ => None, // Unsupported extension
            }
        } else {
            None
        }
    }

    /// Builds and sets the `final_caption` field.
    pub fn build_caption(&mut self, source_url: &Url) {
        let via_link = "https://t.me/crabberbot?start=c";
        let header = format!(
            "<a href=\"{}\">CrabberBot</a> 🦀 <a href=\"{}\">Source</a>",
            via_link, source_url
        );

        let mut quote_parts = Vec::new();
        let uploader = self
            .uploader
            .as_deref()
            .or(self.playlist_uploader.as_deref());
        if let Some(uploader) = uploader {
            if !uploader.is_empty() {
                quote_parts.push(format!("<i>{}</i>", uploader));
            }
        }

        let description = self.description.as_deref().or(self.title.as_deref());
        if let Some(desc) = description {
            let desc = desc.trim();
            if !desc.is_empty() {
                quote_parts.push(desc.to_string());
            }
        }

        let full_quote_content = quote_parts.join("\n");
        // Calculate the space taken by the HTML scaffolding.
        // header + "\n\n" + "<blockquote>" + "</blockquote> + 5 margin"
        let overhead = header.len() + 2 + 12 + 13 + 5;
        let available_space_for_quote = 1024_usize.saturating_sub(overhead);
        let truncated_quote_content: String = full_quote_content
            .chars()
            .take(available_space_for_quote)
            .collect();

        self.final_caption = format!(
            "{}\n\n<blockquote>{}</blockquote>",
            header, truncated_quote_content
        );
    }
}

#[automock]
#[async_trait]
pub trait Downloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaMetadata, DownloadError>;
    async fn download_media(
        &self,
        mut metadata: MediaMetadata,
        url: &Url,
    ) -> Result<MediaMetadata, DownloadError>;
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

    async fn download_media(
        &self,
        mut metadata: MediaMetadata,
        url: &Url,
    ) -> Result<MediaMetadata, DownloadError> {
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
        let mut downloaded_files = HashMap::new();

        for line in stdout_str.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MediaMetadata>(line) {
                Ok(m) => {
                    if let Some(path) = m.filepath {
                        downloaded_files.insert(m.id, path);
                    }
                }
                Err(e) => {
                    log::warn!("Failed to parse a line of yt-dlp JSON output: {}", e);
                }
            }
        }

        if downloaded_files.is_empty() {
            return Err(DownloadError::ParsingFailed(
                "Could not extract any media metadata from yt-dlp output.".to_string(),
            ));
        }

        if let Some(entries) = &mut metadata.entries {
            // Playlist case
            for entry in entries.iter_mut() {
                if let Some(path) = downloaded_files.get(&entry.id) {
                    entry.filepath = Some(path.clone());
                }
            }
            // Also set the filepath for the top-level object for consistency
            if !entries.is_empty() {
                if let Some(path) = downloaded_files.get(&entries[0].id) {
                    metadata.filepath = Some(path.clone());
                }
            }
        } else {
            // Single item case
            if let Some(path) = downloaded_files.get(&metadata.id) {
                metadata.filepath = Some(path.clone());
            }
        }

        Ok(metadata)
    }
}
