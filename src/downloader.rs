use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

const METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Error, Debug, PartialEq)]
pub enum DownloadError {
    #[error("yt-dlp command failed: {0}")]
    CommandFailed(String),
    #[error("Failed to parse yt-dlp output: {0}")]
    ParsingFailed(String),
    #[error("yt-dlp timed out after {0} seconds")]
    Timeout(u64),
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MediaType {
    Video,
    Photo,
}

impl MediaType {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "mp4" | "webm" | "gif" | "mov" | "mkv" => Some(MediaType::Video),
            "jpg" | "jpeg" | "png" | "webp" | "heic" => Some(MediaType::Photo),
            _ => None,
        }
    }
}

/// Pre-download metadata returned by yt-dlp's `--dump-single-json`.
#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct MediaInfo {
    pub id: String,
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
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(rename = "filesize_approx", default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub entries: Option<Vec<MediaInfo>>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// A single downloaded file with its resolved media type.
#[derive(Debug)]
pub struct DownloadedItem {
    pub filepath: PathBuf,
    pub media_type: MediaType,
    pub thumbnail_filepath: Option<PathBuf>,
}

/// Result of a download operation: either a single item or a group.
#[derive(Debug)]
pub enum DownloadedMedia {
    Single(DownloadedItem),
    Group(Vec<DownloadedItem>),
}

/// Lightweight struct for parsing each line of yt-dlp's `--print-json` output.
#[derive(Debug, Deserialize)]
struct DownloadOutputLine {
    id: String,
    #[serde(rename = "_filename")]
    filepath: Option<String>,
    ext: Option<String>,
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Builds a caption string from pre-download metadata and the source URL.
pub fn build_caption(info: &MediaInfo, source_url: &Url) -> String {
    let via_link = "https://t.me/crabberbot?start=c";
    let header = format!(
        "<a href=\"{}\">CrabberBot</a> 🦀 <a href=\"{}\">Source</a>",
        via_link, source_url
    );

    let mut quote_parts = Vec::new();
    let uploader = info
        .uploader
        .as_deref()
        .or(info.playlist_uploader.as_deref());
    if let Some(uploader) = uploader {
        if !uploader.is_empty() {
            quote_parts.push(format!("<i>{}</i>", escape_html(uploader)));
        }
    }

    let description = info.description.as_deref().or(info.title.as_deref());
    if let Some(desc) = description {
        let desc = desc.trim();
        if !desc.is_empty() {
            quote_parts.push(escape_html(desc));
        }
    }

    let full_quote_content = quote_parts.join("\n");
    let overhead = header.chars().count() + 2 + 12 + 13 + 5;
    let available_space_for_quote = 1024_usize.saturating_sub(overhead);
    let final_quote = if full_quote_content.chars().count() > available_space_for_quote {
        let mut truncated: String = full_quote_content
            .chars()
            .take(available_space_for_quote)
            .collect();
        truncated.push_str("[...]");
        truncated
    } else {
        full_quote_content
    };

    format!("{}\n\n<blockquote>{}</blockquote>", header, final_quote)
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Downloader: Send + Sync {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaInfo, DownloadError>;
    async fn download_media(
        &self,
        info: &MediaInfo,
        url: &Url,
    ) -> Result<DownloadedMedia, DownloadError>;
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

    fn build_base_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.yt_dlp_path);
        command.arg("--no-warnings").arg("--ignore-config");
        command
    }

    fn escape_glob(s: &str) -> String {
        let mut escaped = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '*' => escaped.push_str("[*]"),
                '?' => escaped.push_str("[?]"),
                '[' => escaped.push_str("[[]"),
                ']' => escaped.push_str("[]]"),
                _ => escaped.push(c),
            }
        }
        escaped
    }

    /// Finds a thumbnail file written by `--write-thumbnail`, excluding the video file itself.
    fn find_thumbnail(uuid: &str, id: &str, video_filepath: &Path) -> Option<PathBuf> {
        let pattern = format!("./{}.{}.*", uuid, Self::escape_glob(id));
        glob::glob(&pattern)
            .ok()?
            .filter_map(|entry| entry.ok())
            .find(|path| path != video_filepath)
    }
}

impl Default for YtDlpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Downloader for YtDlpDownloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaInfo, DownloadError> {
        log::info!("Fetching metadata for {}", url);

        let mut command = self.build_base_command();
        command.arg("--dump-single-json").arg(url.as_str());

        let output = tokio::time::timeout(METADATA_TIMEOUT, command.output())
            .await
            .map_err(|_| DownloadError::Timeout(METADATA_TIMEOUT.as_secs()))?
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

        serde_json::from_str::<MediaInfo>(&stdout_str).map_err(|e| {
            log::error!("Failed to parse metadata JSON for {}: {}", url, e);
            DownloadError::ParsingFailed(e.to_string())
        })
    }

    async fn download_media(
        &self,
        info: &MediaInfo,
        url: &Url,
    ) -> Result<DownloadedMedia, DownloadError> {
        let uuid = uuid::Uuid::new_v4().to_string();
        let filename_template = format!("./{}.%(id)s.%(ext)s", uuid);
        let is_single_with_thumbnail = info.entries.is_none() && info.thumbnail.is_some();

        log::info!("Downloading {}", url);

        let mut command = self.build_base_command();
        command
            .arg("--print-json")
            .arg("-S vcodec:h264,res,acodec:m4a")
            .arg("-o")
            .arg(&filename_template);

        if is_single_with_thumbnail {
            command.arg("--write-thumbnail");
        }

        command.arg(url.as_str());

        let output = tokio::time::timeout(DOWNLOAD_TIMEOUT, command.output())
            .await
            .map_err(|_| DownloadError::Timeout(DOWNLOAD_TIMEOUT.as_secs()))?
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("yt-dlp failed for url {}: {}", url, stderr);
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let mut downloaded_files: HashMap<String, DownloadOutputLine> = HashMap::new();

        for line in stdout_str.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<DownloadOutputLine>(line) {
                Ok(dl) => {
                    if dl.filepath.is_some() {
                        downloaded_files.insert(dl.id.clone(), dl);
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

        if let Some(entries) = &info.entries {
            let items: Vec<DownloadedItem> = entries
                .iter()
                .filter_map(|entry| {
                    let dl = downloaded_files.get(&entry.id)?;
                    let filepath = dl.filepath.as_ref()?;
                    let ext = dl.ext.as_deref()?;
                    let media_type = MediaType::from_extension(ext)?;
                    Some(DownloadedItem {
                        filepath: PathBuf::from(filepath),
                        media_type,
                        thumbnail_filepath: None,
                    })
                })
                .collect();

            if items.is_empty() {
                return Err(DownloadError::ParsingFailed(
                    "No valid media items found in playlist output.".to_string(),
                ));
            }

            Ok(DownloadedMedia::Group(items))
        } else {
            let dl = downloaded_files.get(&info.id).ok_or_else(|| {
                DownloadError::ParsingFailed(format!("No download output for id {}", info.id))
            })?;
            let filepath_str = dl.filepath.as_ref().ok_or_else(|| {
                DownloadError::ParsingFailed("Download output missing filepath".to_string())
            })?;
            let filepath = PathBuf::from(filepath_str);
            let ext = dl.ext.as_deref().ok_or_else(|| {
                DownloadError::ParsingFailed("Download output missing extension".to_string())
            })?;
            let media_type = MediaType::from_extension(ext).ok_or_else(|| {
                DownloadError::ParsingFailed(format!("Unsupported file extension: {}", ext))
            })?;

            let thumbnail_filepath = if is_single_with_thumbnail {
                Self::find_thumbnail(&uuid, &info.id, &filepath)
            } else {
                None
            };

            Ok(DownloadedMedia::Single(DownloadedItem {
                filepath,
                media_type,
                thumbnail_filepath,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn test_build_caption_normal_text() {
        let info = MediaInfo {
            id: "1".to_string(),
            uploader: Some("TestUser".to_string()),
            description: Some("A normal description".to_string()),
            title: None,
            media_type: None,
            playlist_uploader: None,
            thumbnail: None,
            duration: None,
            filesize: None,
            entries: None,
            resolution: None,
            width: None,
            height: None,
        };
        let url = Url::parse("https://example.com/video").unwrap();
        let caption = build_caption(&info, &url);
        assert!(caption.contains("<i>TestUser</i>"));
        assert!(caption.contains("A normal description"));
    }

    #[test]
    fn test_build_caption_escapes_html_tags() {
        let info = MediaInfo {
            id: "1".to_string(),
            uploader: Some("<script>alert('xss')</script>".to_string()),
            description: Some("desc with <b>tags</b>".to_string()),
            title: None,
            media_type: None,
            playlist_uploader: None,
            thumbnail: None,
            duration: None,
            filesize: None,
            entries: None,
            resolution: None,
            width: None,
            height: None,
        };
        let url = Url::parse("https://example.com/video").unwrap();
        let caption = build_caption(&info, &url);
        assert!(caption.contains("&lt;script&gt;"));
        assert!(caption.contains("&lt;b&gt;tags&lt;/b&gt;"));
        assert!(!caption.contains("<script>"));
        assert!(!caption.contains("<b>tags"));
    }

    #[test]
    fn test_build_caption_escapes_ampersands() {
        let info = MediaInfo {
            id: "1".to_string(),
            uploader: Some("Tom & Jerry".to_string()),
            description: Some("A & B < C > D".to_string()),
            title: None,
            media_type: None,
            playlist_uploader: None,
            thumbnail: None,
            duration: None,
            filesize: None,
            entries: None,
            resolution: None,
            width: None,
            height: None,
        };
        let url = Url::parse("https://example.com/video").unwrap();
        let caption = build_caption(&info, &url);
        assert!(caption.contains("Tom &amp; Jerry"));
        assert!(caption.contains("A &amp; B &lt; C &gt; D"));
        // Verify no double-escaping
        assert!(!caption.contains("&amp;amp;"));
    }

    #[tokio::test]
    async fn test_yt_dlp_uses_custom_path_and_fails_if_invalid() {
        let downloader = YtDlpDownloader {
            yt_dlp_path: "/path/to/a/nonexistent/yt-dlp-binary".to_string(),
        };

        let url = Url::parse("https://example.com").unwrap();

        let result = downloader.get_media_metadata(&url).await;

        assert!(result.is_err());

        match result {
            Err(DownloadError::CommandFailed(msg)) => {
                assert!(msg.contains("No such file or directory"));
            }
            _ => panic!("Expected CommandFailed error, but got something else."),
        }
    }
}
