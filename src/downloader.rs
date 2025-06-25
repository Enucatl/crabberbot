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

// Our serde structs for yt-dlp's JSON output
#[derive(Deserialize)]
struct YtDlpOutput {
    // We only need the description and the list of downloaded files.
    // Use `serde(default)` for fields that might be missing (like description).
    #[serde(default)]
    description: String,

    #[serde(rename = "_filename")]
    filepath: String, // This field appears when you use --print-json, it lists what would be downloaded.
                      // For our case, we will simply download and then find the files.
                      // A more robust approach not shown here might be to parse the JSON first, then download.
                      // For speed, we download and get metadata at the same time.
}

#[automock]
#[async_trait]
pub trait Downloader {
    async fn download_media(&self, url: &str) -> Result<(String, Vec<String>), DownloadError>;
}

// The REAL implementation
pub struct YtDlpDownloader;

#[async_trait]
impl Downloader for YtDlpDownloader {
    async fn download_media(&self, url: &str) -> Result<(String, Vec<String>), DownloadError> {
        let uuid = uuid::Uuid::new_v4().to_string();
        let filename_template = format!("{}.%(id)s.%(ext)s", uuid);

        log::info!("Downloading {}", url);

        // Command to get metadata and download at the same time
        let output = tokio::process::Command::new("yt-dlp")
            .arg("--print-json") // Print metadata for each video to stdout
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

        // 3. Parse the output to get the generated file paths and caption.
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let mut file_paths = Vec::new();
        let mut last_caption = String::new();

        // A single post (like on Instagram) can contain multiple videos/images.
        // yt-dlp will print one JSON object per line for each item.
        for line in stdout_str.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<YtDlpOutput>(line) {
                Ok(metadata) => {
                    log::info!("Successfully downloaded and parsed: {}", metadata.filepath);
                    file_paths.push(metadata.filepath);
                    // We'll use the description from the last media item as the post's caption.
                    last_caption = metadata.description;
                }
                Err(e) => {
                    log::warn!("Failed to parse a line of yt-dlp JSON output: {}", e);
                }
            }
        }

        if file_paths.is_empty() {
            return Err(DownloadError::ParsingFailed(
                "Could not extract any filenames from yt-dlp output.".to_string(),
            ));
        }

        // Telegram caption limit is 1024 chars
        let final_caption = last_caption.chars().take(1024).collect();

        Ok((final_caption, file_paths))
    }
}
