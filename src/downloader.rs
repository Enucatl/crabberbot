use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use url::Url;
use uuid::Uuid;

use crate::validator::MAX_FILESIZE_BYTES;
use crate::worker_protocol::{Request, Response, ResultData, read_frame, write_frame};

const METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Error, Debug, PartialEq)]
pub enum DownloadError {
    #[error("yt-dlp command failed: {0}")]
    CommandFailed(String),
    #[error("Failed to parse yt-dlp output: {0}")]
    ParsingFailed(String),
    #[error("yt-dlp timed out after {0} seconds")]
    Timeout(u64),
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
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
    if let Some(uploader) = uploader
        && !uploader.is_empty()
    {
        quote_parts.push(format!("<i>{}</i>", escape_html_text(uploader)));
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

pub struct SocketDownloader {
    socket_path: PathBuf,
}

impl SocketDownloader {
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    async fn request(&self, request: Request) -> Result<ResultData, DownloadError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| {
                DownloadError::CommandFailed(format!("downloader worker unavailable: {error}"))
            })?;
        write_frame(&mut stream, &request)
            .await
            .map_err(DownloadError::CommandFailed)?;
        let frame = read_frame(&mut stream)
            .await
            .map_err(DownloadError::CommandFailed)?;
        match serde_json::from_slice::<Response>(&frame)
            .map_err(|error| DownloadError::ParsingFailed(error.to_string()))?
        {
            Response::Ok { result } => Ok(result),
            Response::Error { kind, message } if kind == "timeout" => Err(message
                .parse()
                .map(DownloadError::Timeout)
                .unwrap_or(DownloadError::CommandFailed(message))),
            Response::Error { kind, message } if kind == "parsing" => {
                Err(DownloadError::ParsingFailed(message))
            }
            Response::Error { message, .. } => Err(DownloadError::CommandFailed(message)),
        }
    }
}

#[async_trait]
impl Downloader for SocketDownloader {
    async fn get_media_metadata(&self, url: &Url) -> Result<MediaInfo, DownloadError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::CommandFailed(
                "only HTTP(S) URLs are supported".to_string(),
            ));
        }
        match self
            .request(Request::Metadata {
                url: url.to_string(),
            })
            .await?
        {
            ResultData::Metadata { info } => Ok(info),
            _ => Err(DownloadError::ParsingFailed(
                "unexpected downloader response".to_string(),
            )),
        }
    }

    async fn download_media(
        &self,
        info: &MediaInfo,
        url: &Url,
    ) -> Result<DownloadedMedia, DownloadError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::CommandFailed(
                "only HTTP(S) URLs are supported".to_string(),
            ));
        }
        match self
            .request(Request::Download {
                info: info.clone(),
                url: url.to_string(),
            })
            .await?
        {
            ResultData::Download { media } => DownloadedMedia::try_from(media)
                .map_err(|error| DownloadError::ParsingFailed(error.to_string())),
            _ => Err(DownloadError::ParsingFailed(
                "unexpected downloader response".to_string(),
            )),
        }
    }
}

pub struct YtDlpDownloader {
    yt_dlp_path: String,
    download_dir: PathBuf,
    session_limiter: Arc<Semaphore>,
}

impl YtDlpDownloader {
    pub async fn new(yt_dlp_path: String, download_dir: PathBuf, max_sessions: usize) -> Self {
        log::info!("Using yt-dlp executable at: {}", yt_dlp_path);
        log::info!("Using download directory: {}", download_dir.display());

        // Log yt-dlp version
        let mut version_command = tokio::process::Command::new(&yt_dlp_path);
        version_command.arg("--version").kill_on_drop(true);
        if let Ok((_, stdout, _)) = Self::run_command(version_command, METADATA_TIMEOUT).await {
            let version = String::from_utf8_lossy(&stdout);
            log::info!("yt-dlp version: {}", version.trim());
        }

        Self {
            yt_dlp_path,
            download_dir,
            session_limiter: Arc::new(Semaphore::new(max_sessions)),
        }
    }

    fn build_base_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.yt_dlp_path);
        command
            .arg("--no-warnings")
            .arg("--ignore-config")
            .arg("--proxy")
            .arg("http://egress-proxy:3128");
        command.kill_on_drop(true);
        command
    }

    async fn run_command(
        mut command: tokio::process::Command,
        timeout: Duration,
    ) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), DownloadError> {
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| DownloadError::CommandFailed(e.to_string()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut out = tokio::spawn(read_capped(stdout));
        let mut err = tokio::spawn(read_capped(stderr));
        let mut stdout = None;
        let mut stderr = None;
        enum CommandRun {
            Finished(std::process::ExitStatus),
            Failed(DownloadError),
        }
        let outcome = {
            let wait = tokio::time::timeout(timeout, child.wait());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                status = &mut wait => match status {
                    Ok(Ok(status)) => break CommandRun::Finished(status),
                    Ok(Err(e)) => break CommandRun::Failed(DownloadError::CommandFailed(e.to_string())),
                    Err(_) => break CommandRun::Failed(DownloadError::Timeout(timeout.as_secs())),
                },
                result = &mut out, if stdout.is_none() => match result {
                    Ok(Ok(bytes)) => stdout = Some(bytes),
                    result => break CommandRun::Failed(command_output_error(result)),
                },
                result = &mut err, if stderr.is_none() => match result {
                    Ok(Ok(bytes)) => stderr = Some(bytes),
                    result => break CommandRun::Failed(command_output_error(result)),
                },
                }
            }
        };
        let status = match outcome {
            CommandRun::Finished(status) => status,
            CommandRun::Failed(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                out.abort();
                err.abort();
                return Err(error);
            }
        };
        let stdout = match stdout {
            Some(bytes) => bytes,
            None => out
                .await
                .map_err(|e| DownloadError::CommandFailed(e.to_string()))?
                .map_err(|e| DownloadError::CommandFailed(e.to_string()))?,
        };
        let stderr = match stderr {
            Some(bytes) => bytes,
            None => err
                .await
                .map_err(|e| DownloadError::CommandFailed(e.to_string()))?
                .map_err(|e| DownloadError::CommandFailed(e.to_string()))?,
        };
        Ok((status, stdout, stderr))
    }

    fn validated_download_path(download_dir: &Path, uuid: &str, filepath: &str) -> Option<PathBuf> {
        let reported = Path::new(filepath);
        let path = if reported.is_absolute() {
            reported.to_path_buf()
        } else {
            download_dir.join(reported)
        };
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        let directory = download_dir.canonicalize().ok()?;
        if canonical.parent()? != directory || !canonical.file_name()?.to_str()?.starts_with(uuid) {
            return None;
        }
        Some(canonical)
    }

    /// Finds a thumbnail file written by `--write-thumbnail`, excluding the video file itself.
    fn find_thumbnail(
        download_dir: &Path,
        uuid: &str,
        id: &str,
        video_filepath: &Path,
    ) -> Option<PathBuf> {
        let prefix = format!("{uuid}.{id}.");
        std::fs::read_dir(download_dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find_map(|path| {
                (path != video_filepath
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix)))
                .then(|| Self::validated_download_path(download_dir, uuid, path.to_str()?))
                .flatten()
                .filter(|path| path != video_filepath)
            })
    }

    async fn cleanup_download_artifacts(download_dir: &Path, uuid: &str) {
        let mut entries = match tokio::fs::read_dir(download_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                log::warn!(
                    "Failed to read downloads dir for cleanup {}: {}",
                    download_dir.display(),
                    e
                );
                return;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let should_remove =
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(uuid) && name.as_bytes().get(36) == Some(&b'.')
                    });
            if !should_remove {
                continue;
            }
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

    async fn cleanup_unaccepted_artifacts(
        download_dir: &Path,
        uuid: &str,
        accepted: &[DownloadedItem],
    ) {
        let accepted: Vec<PathBuf> = accepted.iter().map(|item| item.filepath.clone()).collect();
        let mut entries = match tokio::fs::read_dir(download_dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(uuid))
                && !accepted.contains(&path.canonicalize().unwrap_or(path.clone()))
            {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }
}

fn command_output_error(
    result: Result<Result<Vec<u8>, std::io::Error>, tokio::task::JoinError>,
) -> DownloadError {
    match result {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::FileTooLarge => {
            DownloadError::CommandFailed("yt-dlp output exceeded 1 MiB".to_string())
        }
        Ok(Err(e)) => DownloadError::CommandFailed(e.to_string()),
        Err(e) => DownloadError::CommandFailed(e.to_string()),
        Ok(Ok(_)) => {
            DownloadError::CommandFailed("yt-dlp output stream closed unexpectedly".to_string())
        }
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > MAX_COMMAND_OUTPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "output exceeds cap",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
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

        let _permit = self
            .session_limiter
            .acquire()
            .await
            .expect("yt-dlp session limiter closed");
        let (status, stdout, stderr) = Self::run_command(command, METADATA_TIMEOUT).await?;

        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            log::error!(
                "yt-dlp --dump-single-json failed for url {}: {}",
                url,
                stderr
            );
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&stdout);
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
            .arg("--max-filesize")
            .arg(MAX_FILESIZE_BYTES.to_string())
            .arg("-o")
            .arg(&filename_template);

        if is_single_with_thumbnail {
            command
                .arg("--write-thumbnail")
                .arg("-o")
                .arg(&thumbnail_template);
        }

        command.arg(url.as_str());

        let _permit = self
            .session_limiter
            .acquire()
            .await
            .expect("yt-dlp session limiter closed");
        let (status, stdout, stderr) = match Self::run_command(command, DOWNLOAD_TIMEOUT).await {
            Ok(output) => output,
            Err(error) => {
                Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                return Err(error);
            }
        };

        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            log::error!("yt-dlp failed for url {}: {}", url, stderr);
            Self::cleanup_download_artifacts(&download_dir, &uuid).await;
            return Err(DownloadError::CommandFailed(stderr.to_string()));
        }

        let stdout_str = String::from_utf8_lossy(&stdout);
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
                        filepath: Self::validated_download_path(&download_dir, &uuid, filepath)?,
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

            if items.len() < downloaded_files.len() {
                Self::cleanup_unaccepted_artifacts(&download_dir, &uuid, &items).await;
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
            let filepath = match Self::validated_download_path(&download_dir, &uuid, filepath_str) {
                Some(path) => path,
                None => {
                    Self::cleanup_download_artifacts(&download_dir, &uuid).await;
                    return Err(DownloadError::ParsingFailed(
                        "Invalid download path".to_string(),
                    ));
                }
            };
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
            session_limiter: Arc::new(Semaphore::new(4)),
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
    fn test_yt_dlp_session_limiter_uses_configured_permits() {
        let downloader = YtDlpDownloader {
            yt_dlp_path: "yt-dlp".to_string(),
            download_dir: PathBuf::from("/downloads"),
            session_limiter: Arc::new(Semaphore::new(2)),
        };

        let first = downloader.session_limiter.try_acquire().unwrap();
        let second = downloader.session_limiter.try_acquire().unwrap();
        assert!(downloader.session_limiter.try_acquire().is_err());

        drop(first);
        assert!(downloader.session_limiter.try_acquire().is_ok());
        drop(second);
    }

    #[tokio::test]
    async fn socket_downloader_rejects_non_http_urls_before_connecting() {
        let downloader = SocketDownloader::new(PathBuf::from("/no/worker.sock"));
        let error = downloader
            .get_media_metadata(&Url::parse("file:///tmp/video").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, DownloadError::CommandFailed(message) if message.contains("HTTP(S)"))
        );
    }

    #[test]
    fn test_download_path_requires_uuid_file_in_downloads_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let uuid = Uuid::new_v4().to_string();
        let file = temp_dir.path().join(format!("{uuid}.video.mp4"));
        std::fs::write(&file, b"video").unwrap();
        assert_eq!(
            YtDlpDownloader::validated_download_path(
                temp_dir.path(),
                &uuid,
                file.to_str().unwrap()
            ),
            Some(file.canonicalize().unwrap())
        );
        assert!(
            YtDlpDownloader::validated_download_path(temp_dir.path(), &uuid, "../outside.mp4")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_download_path_rejects_symlink() {
        let temp_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let uuid = Uuid::new_v4().to_string();
        let link = temp_dir.path().join(format!("{uuid}.video.mp4"));
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(
            YtDlpDownloader::validated_download_path(
                temp_dir.path(),
                &uuid,
                link.to_str().unwrap()
            )
            .is_none()
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_download_passes_size_limit_and_cleans_failed_artifacts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let script = temp_dir.path().join("fake-yt-dlp");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$previous\" = --max-filesize ]; then echo \"$arg\" > max-filesize; fi\n  if [ \"$previous\" = -o ]; then output=$arg; fi\n  previous=$arg\ndone\nuuid=${output%%.*}\ntouch \"$uuid.media.mp4.part\"\necho 'Maximum file size exceeded' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let downloader = YtDlpDownloader {
            yt_dlp_path: script.to_string_lossy().into_owned(),
            download_dir: temp_dir.path().to_path_buf(),
            session_limiter: Arc::new(Semaphore::new(1)),
        };
        let info = MediaInfo {
            id: "media".to_string(),
            media_type: Some("video".to_string()),
            duration: Some(1.0),
            filesize: Some(1),
            ..Default::default()
        };

        let result = downloader
            .download_media(&info, &Url::parse("https://example.com/video").unwrap())
            .await;

        assert!(matches!(result, Err(DownloadError::CommandFailed(_))));
        assert_eq!(
            std::fs::read_to_string(temp_dir.path().join("max-filesize"))
                .unwrap()
                .trim(),
            MAX_FILESIZE_BYTES.to_string()
        );
        assert!(std::fs::read_dir(temp_dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")
        }));
    }
}
