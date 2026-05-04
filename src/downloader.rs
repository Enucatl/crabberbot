use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

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
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "mp4" | "webm" | "gif" | "mov" | "mkv" => Some(MediaType::Video),
            "jpg" | "jpeg" | "png" | "webp" | "heic" => Some(MediaType::Photo),
            _ => None,
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Video => write!(f, "video"),
            Self::Photo => write!(f, "photo"),
        }
    }
}

impl FromStr for MediaType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "video" => Ok(Self::Video),
            "photo" => Ok(Self::Photo),
            _ => Err(()),
        }
    }
}

/// Pre-download metadata returned by yt-dlp's `--dump-single-json`.
#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
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

#[must_use]
fn escape_html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Builds a caption string from pre-download metadata and the source URL.
#[must_use]
pub fn build_caption(info: &MediaInfo, source_url: &Url) -> String {
    const CAPTION_MAX_LEN: usize = 1024;
    const BLOCKQUOTE_OPEN: &str = "<blockquote>";
    const BLOCKQUOTE_CLOSE: &str = "</blockquote>";
    const TRUNCATION_MARKER: &str = "[...]";
    const SEPARATOR: &str = "\n\n";

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
            quote_parts.push(format!("<i>{}</i>", escape_html_text(uploader)));
        }
    }

    let description = info.description.as_deref().or(info.title.as_deref());
    if let Some(desc) = description {
        let desc = desc.trim();
        if !desc.is_empty() {
            quote_parts.push(escape_html_text(desc));
        }
    }

    let full_quote_content = quote_parts.join("\n");
    let overhead = header.chars().count()
        + SEPARATOR.len()
        + BLOCKQUOTE_OPEN.len()
        + BLOCKQUOTE_CLOSE.len()
        + TRUNCATION_MARKER.len();
    let available_space_for_quote = CAPTION_MAX_LEN.saturating_sub(overhead);
    let final_quote = if full_quote_content.chars().count() > available_space_for_quote {
        let mut truncated: String = full_quote_content
            .chars()
            .take(available_space_for_quote)
            .collect();
        truncated.push_str(TRUNCATION_MARKER);
        truncated
    } else {
        full_quote_content
    };

    format!("{header}{SEPARATOR}{BLOCKQUOTE_OPEN}{final_quote}{BLOCKQUOTE_CLOSE}")
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
    download_dir: PathBuf,
}

impl YtDlpDownloader {
    pub async fn new(yt_dlp_path: String, download_dir: PathBuf) -> Self {
        log::info!("Using yt-dlp executable at: {}", yt_dlp_path);
        log::info!("Using download directory: {}", download_dir.display());

        // Log yt-dlp version
        if let Ok(output) = tokio::process::Command::new(&yt_dlp_path)
            .arg("--version")
            .output()
            .await
        {
            let version = String::from_utf8_lossy(&output.stdout);
            log::info!("yt-dlp version: {}", version.trim());
        }

        // Log available impersonate targets to verify curl_cffi is working
        match tokio::process::Command::new(&yt_dlp_path)
            .arg("--list-impersonate-targets")
            .output()
            .await
        {
            Ok(output) => {
                if output.status.success() {
                    let targets = String::from_utf8_lossy(&output.stdout);
                    log::info!(
                        "yt-dlp impersonate targets available (curl_cffi working):\n{}",
                        targets.trim()
                    );
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log::warn!(
                        "yt-dlp --list-impersonate-targets failed (curl_cffi may not be installed): {}",
                        stderr.trim()
                    );
                }
            }
            Err(e) => {
                log::warn!("Failed to check impersonate targets: {}", e);
            }
        }

        Self {
            yt_dlp_path,
            download_dir,
        }
    }

    fn build_base_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.yt_dlp_path);
        command
            .arg("--no-warnings")
            .arg("--ignore-config")
            .arg("--impersonate")
            .arg("chrome");
        command.kill_on_drop(true);
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

    fn resolve_download_path(download_dir: &Path, filepath: &str) -> PathBuf {
        let path = PathBuf::from(filepath);
        if path.is_absolute() {
            path
        } else {
            let relative_path = path
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(PathBuf::from(part)),
                    Component::CurDir
                    | Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_) => None,
                })
                .collect::<PathBuf>();
            download_dir.join(relative_path)
        }
    }

    /// Finds a thumbnail file written by `--write-thumbnail`, excluding the video file itself.
    fn find_thumbnail(
        download_dir: &Path,
        uuid: &str,
        id: &str,
        video_filepath: &Path,
    ) -> Option<PathBuf> {
        let pattern = download_dir
            .join(format!("{}.{}.*", uuid, Self::escape_glob(id)))
            .to_string_lossy()
            .into_owned();
        glob::glob(&pattern)
            .ok()?
            .filter_map(|entry| entry.ok())
            .find(|path| path != video_filepath)
    }

    async fn cleanup_download_artifacts(download_dir: &Path, uuid: &str) {
        let pattern = download_dir
            .join(format!("{}.*", Self::escape_glob(uuid)))
            .to_string_lossy()
            .into_owned();
        let paths = match glob::glob(&pattern) {
            Ok(paths) => paths,
            Err(e) => {
                log::warn!("Failed to build cleanup glob for {}: {}", uuid, e);
                return;
            }
        };

        for path in paths.filter_map(|entry| entry.ok()) {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => log::info!("Removed incomplete download artifact: {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!(
                    "Failed to remove incomplete download artifact {}: {}",
                    path.display(),
                    e
                ),
            }
        }
    }
}

/// Remove media files left in the downloads directory by older crashed or timed-out runs.
///
/// Normal in-flight downloads are UUID-prefixed and live at the top level of
/// `download_dir`; durable caches live in subdirectories and are intentionally skipped.
pub async fn cleanup_orphaned_downloads(download_dir: &Path) -> usize {
    let mut removed = 0usize;
    let mut entries = match tokio::fs::read_dir(download_dir).await {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!(
                "Failed to read downloads dir for startup cleanup {}: {}",
                download_dir.display(),
                e
            );
            return 0;
        }
    };

    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let path = entry.path();
                let is_file = entry
                    .file_type()
                    .await
                    .is_ok_and(|file_type| file_type.is_file());
                let should_remove = is_file
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_download_artifact_name);
                if !should_remove {
                    continue;
                }

                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {
                        removed += 1;
                        log::info!("Removed orphaned download artifact: {}", path.display());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => log::warn!(
                        "Failed to remove orphaned download artifact {}: {}",
                        path.display(),
                        e
                    ),
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::warn!("Error reading downloads dir during startup cleanup: {}", e);
                break;
            }
        }
    }

    removed
}

fn is_download_artifact_name(filename: &str) -> bool {
    let Some((prefix, rest)) = filename.split_once('.') else {
        return false;
    };
    if Uuid::parse_str(prefix).is_err() {
        return false;
    }

    if rest.ends_with(".part") {
        return true;
    }

    let Some(extension) = rest.rsplit('.').next().map(str::to_ascii_lowercase) else {
        return false;
    };
    MediaType::from_extension(&extension).is_some() || extension == "image"
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
        log::debug!(
            "yt-dlp metadata stdout length for {}: {} bytes",
            url,
            stdout_str.len()
        );

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
        let download_dir = self.download_dir.clone();
        let filename_template = format!("{}.%(id)s.%(ext)s", uuid);
        let thumbnail_template = format!("thumbnail:{}.%(id)s.%(ext)s", uuid);
        let is_single_with_thumbnail = info.entries.is_none() && info.thumbnail.is_some();

        log::info!("Downloading {}", url);

        let mut command = self.build_base_command();
        command
            .current_dir(&download_dir)
            .arg("--print-json")
            .arg("-S")
            .arg("vcodec:h264,res,acodec:m4a")
            .arg("-o")
            .arg(&filename_template);

        if is_single_with_thumbnail {
            command
                .arg("--write-thumbnail")
                .arg("-o")
                .arg(&thumbnail_template);
        }

        command.arg(url.as_str());

        let output = match tokio::time::timeout(DOWNLOAD_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                return Err(DownloadError::CommandFailed(e.to_string()));
            }
            Err(_) => {
                Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                return Err(DownloadError::Timeout(DOWNLOAD_TIMEOUT.as_secs()));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("yt-dlp failed for url {}: {}", url, stderr);
            Self::cleanup_download_artifacts(&download_dir, &uuid).await;
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
            Self::cleanup_download_artifacts(&download_dir, &uuid).await;
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
                        filepath: Self::resolve_download_path(&download_dir, filepath),
                        media_type,
                        thumbnail_filepath: None,
                    })
                })
                .collect();

            if items.is_empty() {
                Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                return Err(DownloadError::ParsingFailed(
                    "No valid media items found in playlist output.".to_string(),
                ));
            }

            Ok(DownloadedMedia::Group(items))
        } else {
            let dl = match downloaded_files.get(&info.id) {
                Some(dl) => dl,
                None => {
                    Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                    return Err(DownloadError::ParsingFailed(format!(
                        "No download output for id {}",
                        info.id
                    )));
                }
            };
            let filepath_str = match dl.filepath.as_ref() {
                Some(filepath) => filepath,
                None => {
                    Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                    return Err(DownloadError::ParsingFailed(
                        "Download output missing filepath".to_string(),
                    ));
                }
            };
            let filepath = Self::resolve_download_path(&download_dir, filepath_str);
            let ext = match dl.ext.as_deref() {
                Some(ext) => ext,
                None => {
                    Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                    return Err(DownloadError::ParsingFailed(
                        "Download output missing extension".to_string(),
                    ));
                }
            };
            let media_type = match MediaType::from_extension(ext) {
                Some(media_type) => media_type,
                None => {
                    Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                    return Err(DownloadError::ParsingFailed(format!(
                        "Unsupported file extension: {}",
                        ext
                    )));
                }
            };

            let thumbnail_filepath = if is_single_with_thumbnail {
                Self::find_thumbnail(&download_dir, &uuid, &info.id, &filepath)
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            download_dir: PathBuf::from("/downloads"),
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

    #[test]
    fn test_resolve_download_path_keeps_absolute_paths() {
        let download_dir = Path::new("/downloads");
        let filepath = "/downloads/video.mp4";

        let resolved = YtDlpDownloader::resolve_download_path(download_dir, filepath);

        assert_eq!(resolved, PathBuf::from("/downloads/video.mp4"));
    }

    #[test]
    fn test_resolve_download_path_rebases_relative_paths_under_downloads_dir() {
        let download_dir = Path::new("/downloads");
        let filepath = "./video.mp4";

        let resolved = YtDlpDownloader::resolve_download_path(download_dir, filepath);

        assert_eq!(resolved, PathBuf::from("/downloads/video.mp4"));
    }

    #[test]
    fn test_resolve_download_path_does_not_allow_relative_escape() {
        let download_dir = Path::new("/downloads");
        let filepath = "../video.mp4";

        let resolved = YtDlpDownloader::resolve_download_path(download_dir, filepath);

        assert_eq!(resolved, PathBuf::from("/downloads/video.mp4"));
    }

    #[test]
    fn test_find_thumbnail_searches_downloads_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let download_dir = temp_dir.path();
        let video_filepath = download_dir.join("test-id.media.mp4");
        let thumbnail_filepath = download_dir.join("test-id.media.jpg");
        std::fs::write(&video_filepath, b"video").unwrap();
        std::fs::write(&thumbnail_filepath, b"thumbnail").unwrap();

        let found =
            YtDlpDownloader::find_thumbnail(download_dir, "test-id", "media", &video_filepath);

        assert_eq!(found, Some(thumbnail_filepath));
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_downloads_removes_uuid_media_artifacts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let download_dir = temp_dir.path();
        let uuid = uuid::Uuid::new_v4();
        let video = download_dir.join(format!("{uuid}.media.mp4"));
        let thumbnail = download_dir.join(format!("{uuid}.media.jpg"));
        let partial = download_dir.join(format!("{uuid}.media.mp4.part"));
        let tiktok_image = download_dir.join(format!("{uuid}.media.image"));
        let unrelated = download_dir.join("keep.mp4");
        let cache_dir = download_dir.join("audio_cache");
        let cached_audio = cache_dir.join(format!("{uuid}.mp3"));

        std::fs::create_dir(&cache_dir).unwrap();
        for path in [
            &video,
            &thumbnail,
            &partial,
            &tiktok_image,
            &unrelated,
            &cached_audio,
        ] {
            std::fs::write(path, b"data").unwrap();
        }

        let removed = cleanup_orphaned_downloads(download_dir).await;

        assert_eq!(removed, 4);
        assert!(!video.exists());
        assert!(!thumbnail.exists());
        assert!(!partial.exists());
        assert!(!tiktok_image.exists());
        assert!(unrelated.exists());
        assert!(cached_audio.exists());
    }

    #[tokio::test]
    async fn test_cleanup_download_artifacts_removes_only_matching_uuid() {
        let temp_dir = tempfile::tempdir().unwrap();
        let download_dir = temp_dir.path();
        let target_uuid = uuid::Uuid::new_v4().to_string();
        let other_uuid = uuid::Uuid::new_v4();
        let target_video = download_dir.join(format!("{target_uuid}.media.mp4"));
        let target_part = download_dir.join(format!("{target_uuid}.media.mp4.part"));
        let other_video = download_dir.join(format!("{other_uuid}.media.mp4"));

        for path in [&target_video, &target_part, &other_video] {
            std::fs::write(path, b"data").unwrap();
        }

        YtDlpDownloader::cleanup_download_artifacts(download_dir, &target_uuid).await;

        assert!(!target_video.exists());
        assert!(!target_part.exists());
        assert!(other_video.exists());
    }
}
