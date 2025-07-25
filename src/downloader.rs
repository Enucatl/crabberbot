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
    #[error("Could not find downloaded thumbnail: {0}")]
    ThumbnailError(String),
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct MediaMetadata {
    // ---- Fields for Post-Download Info
    pub id: String,
    #[serde(rename = "_filename")]
    pub filepath: Option<String>,
    pub ext: Option<String>,
    pub thumbnail_filepath: Option<String>,

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
    #[serde(default)]
    pub thumbnail: Option<String>,

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
            log::info!("file extension {}", &ext);
            match ext.as_str() {
                "mp4" | "webm" | "gif" | "mov" | "mkv" => Some("video"),
                "jpg" | "jpeg" | "png" | "webp" | "heic" => Some("photo"),
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
        // header + "\n\n" + "<blockquote>" + "</blockquote> + 5 margin for [...]"
        let overhead = header.len() + 2 + 12 + 13 + 5;
        let available_space_for_quote = 1024_usize.saturating_sub(overhead);
        let final_caption = if full_quote_content.len() > available_space_for_quote {
            let mut truncated_quote_content: String = full_quote_content
                .chars()
                .take(available_space_for_quote)
                .collect();
            truncated_quote_content.push_str("[...]");
            truncated_quote_content
        } else {
            full_quote_content
        };

        self.final_caption = format!("{}\n\n<blockquote>{}</blockquote>", header, final_caption);
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
    async fn download_thumbnail(
        &self,
        metadata: &MediaMetadata,
        url: &Url,
    ) -> Result<Option<String>, DownloadError>;
}

pub struct YtDlpDownloader {
    yt_dlp_path: String,
}

impl YtDlpDownloader {
    pub fn new() -> Self {
        let yt_dlp_path = std::env::var("YT_DLP_PATH").unwrap_or_else(|_| "yt-dlp".to_string());
        log::info!("Using yt-dlp executable at: {}", yt_dlp_path);
        Self { yt_dlp_path }
    }

    /// Helper function to create a base `yt-dlp` command with common arguments.
    fn build_base_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.yt_dlp_path);
        command.arg("--no-warnings").arg("--ignore-config");
        command
    }
}

// Implement `Default` to make instantiation cleaner when no custom config is needed.
impl Default for YtDlpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Downloader for YtDlpDownloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaMetadata, DownloadError> {
        log::info!("Fetching metadata for {}", url);

        let mut command = self.build_base_command();
        command.arg("--dump-single-json").arg(url.as_str());

        let output = command
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
        // Prepending with `./` is a good practice to ensure the file is created in the
        // current working directory, avoiding ambiguity.
        let filename_template = format!("./{}.%(id)s.%(ext)s", uuid);

        log::info!("Downloading {}", url);

        let mut command = self.build_base_command();
        // -S flag to sort format and avoid webm video which can't be played by telegram
        // https://github.com/yt-dlp/yt-dlp/issues/8322#issuecomment-1755932331
        command
            .arg("--print-json")
            .arg("-S vcodec:h264,res,acodec:m4a")
            .arg("-o")
            .arg(&filename_template)
            .arg(url.as_str());

        let output = command
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
                if let Some(path) = self.download_thumbnail(&metadata, url).await? {
                    metadata.thumbnail_filepath = Some(path);
                }
            }
        }

        Ok(metadata)
    }

    /// Downloads only the thumbnail for a given video URL.
    /// Returns the path to the downloaded thumbnail if successful.
    async fn download_thumbnail(
        &self,
        metadata: &MediaMetadata,
        url: &Url,
    ) -> Result<Option<String>, DownloadError> {
        // 1. Only proceed if a thumbnail is expected to exist.
        if metadata.thumbnail.is_none() {
            return Ok(None);
        }

        log::info!("Attempting to download thumbnail for {}...", &metadata.id);

        // 3. Build the command to download *only* the thumbnail.
        let filename_template = "./thumbnail.%(id)s.%(ext)s";
        let mut command = self.build_base_command();
        command
            .arg("--write-thumbnail")
            .arg("--skip-download")
            .arg("-o")
            .arg(&filename_template)
            .arg(url.as_str());

        // 4. Execute the command and check for errors.
        let output = command
            .output()
            .await
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            // Log the error from yt-dlp but don't crash; maybe the thumbnail is gone.
            log::error!(
                "yt-dlp failed to download thumbnail for {}:\n{}",
                &metadata.id,
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(None); // Return Ok(None) to indicate non-fatal failure.
        }

        // 5. Find the actual file that was created. We use a glob pattern
        // because we don't know if the extension will be .jpg, .webp, .png, etc.
        let pattern = format!("thumbnail.{}.*", &metadata.id);

        // Use the glob crate to find the one matching file
        if let Some(found_path) = glob::glob(&pattern)
            .map_err(|e| DownloadError::ThumbnailError(e.to_string()))?
            .next()
        {
            let thumbnail_path =
                found_path.map_err(|e| DownloadError::ThumbnailError(e.to_string()))?;
            log::info!(
                "Successfully downloaded thumbnail to: {:?}",
                &thumbnail_path
            );
            if let Some(thumbnail_path_str) = thumbnail_path.to_str() {
                Ok(Some(thumbnail_path_str.to_string()))
            } else {
                log::error!(
                    "couldn't get the thumbnail path as string to: {:?}",
                    &thumbnail_path
                );
                Err(DownloadError::ThumbnailError(
                    "couldn't get the thumbnail path".to_string(),
                ))
            }
        } else {
            log::error!(
                "Thumbnail downloaded, but could not find the output file for pattern: {}",
                pattern
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    // This test confirms that the downloader attempts to use the path provided
    // during its creation. We provide a path we know doesn't exist.
    #[tokio::test]
    async fn test_yt_dlp_uses_custom_path_and_fails_if_invalid() {
        // This path is intentionally invalid.
        let downloader = YtDlpDownloader {
            yt_dlp_path: "/path/to/a/nonexistent/yt-dlp-binary".to_string(),
        };

        let url = Url::parse("https://example.com").unwrap();

        let result = downloader.get_media_metadata(&url).await;

        // We expect the operation to fail because the command cannot be found.
        assert!(result.is_err());

        // We can also be more specific about the error type.
        match result {
            Err(DownloadError::CommandFailed(msg)) => {
                // The error message from the OS will contain something like "No such file or directory"
                // This proves that it tried to execute the specific, invalid path.
                assert!(msg.contains("No such file or directory"));
            }
            _ => panic!("Expected CommandFailed error, but got something else."),
        }
    }
}
