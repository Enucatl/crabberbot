use std::path::PathBuf;
use std::sync::Arc;

use crabberbot::downloader::{
    DownloadError, Downloader, YtDlpDownloader, cleanup_orphaned_downloads,
};
use crabberbot::premium::audio_extractor::{
    AudioExtractionError, AudioExtractor, FfmpegAudioExtractor,
};
use crabberbot::worker_protocol::{
    Request, Response, ResultData, WireMedia, read_frame, write_frame,
};
use tokio::net::{UnixListener, UnixStream};
use url::Url;

fn response_error(kind: &str, message: impl ToString) -> Response {
    Response::Error {
        kind: kind.to_string(),
        message: message.to_string(),
    }
}

fn download_error(download_error: DownloadError) -> Response {
    match download_error {
        DownloadError::Timeout(seconds) => response_error("timeout", seconds),
        DownloadError::MediaUnavailable(message) => response_error("unavailable", message),
        DownloadError::ParsingFailed(message) => response_error("parsing", message),
        DownloadError::CommandFailed(message) => response_error("command", message),
    }
}

fn audio_error(audio_error: AudioExtractionError) -> Response {
    match audio_error {
        AudioExtractionError::Timeout(seconds) => response_error("timeout", seconds),
        AudioExtractionError::ParseError(message) => response_error("parse", message),
        AudioExtractionError::FfprobeError(message)
        | AudioExtractionError::FfmpegError(message) => response_error("ffmpeg", message),
    }
}

fn http_url(raw: &str) -> Result<Url, Response> {
    let url = raw
        .parse::<Url>()
        .map_err(|_| response_error("command", "invalid URL"))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(url)
    } else {
        Err(response_error("command", "only HTTP(S) URLs are supported"))
    }
}

async fn handle(
    mut stream: UnixStream,
    downloader: Arc<YtDlpDownloader>,
    audio: Arc<FfmpegAudioExtractor>,
    downloads_dir: PathBuf,
) {
    let response = match read_frame(&mut stream)
        .await
        .and_then(|frame| serde_json::from_slice::<Request>(&frame).map_err(|e| e.to_string()))
    {
        Ok(Request::Metadata { url }) => match http_url(&url) {
            Ok(url) => downloader
                .get_media_metadata(&url)
                .await
                .map(|info| Response::Ok {
                    result: ResultData::Metadata { info },
                })
                .unwrap_or_else(download_error),
            Err(response) => response,
        },
        Ok(Request::Download { info, url }) => match http_url(&url) {
            Ok(url) => downloader
                .download_media(&info, &url)
                .await
                .map(|media| Response::Ok {
                    result: ResultData::Download {
                        media: WireMedia::from(media),
                    },
                })
                .unwrap_or_else(download_error),
            Err(response) => response,
        },
        Ok(Request::ExtractAudio {
            video_path,
            title,
            author,
        }) => {
            let path = PathBuf::from(video_path);
            let valid = path
                .canonicalize()
                .ok()
                .is_some_and(|path| path.parent() == downloads_dir.canonicalize().ok().as_deref());
            if !valid {
                response_error("ffmpeg", "invalid video path")
            } else {
                audio
                    .extract_audio(&path, title, author)
                    .await
                    .map(|result| Response::Ok {
                        result: ResultData::Audio {
                            audio_path: result.audio_path.to_string_lossy().into_owned(),
                            duration_secs: result.duration_secs,
                        },
                    })
                    .unwrap_or_else(audio_error)
            }
        }
        Err(message) => response_error("command", format!("invalid downloader request: {message}")),
    };
    let _ = write_frame(&mut stream, &response).await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();
    let socket = PathBuf::from(
        std::env::var("DOWNLOADER_SOCKET")
            .unwrap_or_else(|_| "/downloader/downloader.sock".to_string()),
    );
    let downloads =
        PathBuf::from(std::env::var("DOWNLOADS_DIR").unwrap_or_else(|_| "/downloads".to_string()));
    let audio_cache = PathBuf::from(
        std::env::var("AUDIO_CACHE_DIR").unwrap_or_else(|_| "/downloads/audio_cache".to_string()),
    );
    let sessions = std::env::var("MAX_YT_DLP_SESSIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value: &usize| *value > 0)
        .unwrap_or(4);
    std::fs::create_dir_all(socket.parent().ok_or("socket has no parent")?)?;
    std::fs::create_dir_all(&downloads)?;
    std::fs::create_dir_all(&audio_cache)?;
    let _ = std::fs::remove_file(&socket);
    let removed = cleanup_orphaned_downloads(&downloads).await;
    let downloader =
        Arc::new(YtDlpDownloader::new("yt-dlp".to_string(), downloads.clone(), sessions).await);
    let audio = Arc::new(FfmpegAudioExtractor::new(3, audio_cache));
    let listener = UnixListener::bind(&socket)?;
    log::info!(
        "downloader worker listening on {}, removed {} orphan(s)",
        socket.display(),
        removed
    );
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle(
            stream,
            downloader.clone(),
            audio.clone(),
            downloads.clone(),
        ));
    }
}
