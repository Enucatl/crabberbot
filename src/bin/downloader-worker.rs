use std::path::{Path, PathBuf};
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
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use url::Url;
use uuid::Uuid;

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
    let request = read_frame(&mut stream)
        .await
        .and_then(|frame| serde_json::from_slice::<Request>(&frame).map_err(|e| e.to_string()));
    let response = tokio::select! {
        response = process_request(request, &downloader, &audio, &downloads_dir) => response,
        _ = stream.read_u8() => return,
    };
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        write_frame(&mut stream, &response).await.is_ok() && matches!(stream.read_u8().await, Ok(1))
    })
    .await
    .unwrap_or(false);
    if !delivered {
        cleanup_undelivered(&response, &downloads_dir).await;
    }
}

async fn process_request(
    request: Result<Request, String>,
    downloader: &YtDlpDownloader,
    audio: &FfmpegAudioExtractor,
    downloads_dir: &Path,
) -> Response {
    match request {
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
    }
}

async fn cleanup_undelivered(response: &Response, downloads_dir: &Path) {
    match response {
        Response::Ok {
            result: ResultData::Download { media },
        } => {
            let path = match media {
                WireMedia::Single { item } => Some(&item.filepath),
                WireMedia::Group { items } => items.first().map(|item| &item.filepath),
            };
            if let Some(uuid) = path
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| name.split_once('.'))
                .and_then(|(prefix, _)| Uuid::parse_str(prefix).ok())
            {
                YtDlpDownloader::cleanup_download_artifacts(downloads_dir, &uuid.to_string()).await;
            }
        }
        Response::Ok {
            result: ResultData::Audio { audio_path, .. },
        } => {
            let _ = tokio::fs::remove_file(audio_path).await;
        }
        _ => {}
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crabberbot::downloader::MediaInfo;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    async fn wait_for(path: &Path) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake downloader did not reach expected step");
    }

    fn no_artifacts(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().all(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .split_once('.')
                .is_none_or(|(prefix, _)| Uuid::parse_str(prefix).is_err())
        })
    }

    async fn wait_for_cleanup(dir: &Path) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !no_artifacts(dir) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("UUID artifact survived disconnect");
    }

    async fn start_request(
        downloader: Arc<YtDlpDownloader>,
        dir: &Path,
    ) -> (UnixStream, tokio::task::JoinHandle<()>) {
        let (mut client, worker) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle(
            worker,
            downloader,
            Arc::new(FfmpegAudioExtractor::new(1, dir.to_path_buf())),
            dir.to_path_buf(),
        ));
        write_frame(
            &mut client,
            &Request::Download {
                info: MediaInfo {
                    id: "media".to_string(),
                    ..Default::default()
                },
                url: "https://example.com/video".to_string(),
            },
        )
        .await
        .unwrap();
        (client, task)
    }

    #[tokio::test]
    async fn disconnect_cancels_queued_and_running_downloads_and_unacked_result() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-yt-dlp");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake; exit 0; fi\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --dump-single-json ]; then touch \"$(dirname \"$0\")/metadata-started\"; sleep 30; exit 0; fi\n  if [ \"$previous\" = -o ]; then output=$arg; fi\n  previous=$arg\ndone\nuuid=${output%%.*}\ntouch download-started \"$uuid.media.mp4.part\"\nwhile [ ! -f finish ]; do sleep 0.1; done\nmv \"$uuid.media.mp4.part\" \"$uuid.media.mp4\"\nprintf '{\"id\":\"media\",\"_filename\":\"%s/%s.media.mp4\",\"ext\":\"mp4\"}\\n' \"$PWD\" \"$uuid\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        let downloader = Arc::new(
            YtDlpDownloader::new(
                script.to_string_lossy().into_owned(),
                dir.path().to_path_buf(),
                1,
            )
            .await,
        );

        let metadata_downloader = downloader.clone();
        let metadata = tokio::spawn(async move {
            metadata_downloader
                .get_media_metadata(&Url::parse("https://example.com").unwrap())
                .await
        });
        wait_for(&dir.path().join("metadata-started")).await;
        let (queued_client, queued_task) = start_request(downloader.clone(), dir.path()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(queued_client);
        queued_task.await.unwrap();
        metadata.abort();
        let _ = metadata.await;
        assert!(!dir.path().join("download-started").exists());
        assert!(no_artifacts(dir.path()));

        let (running_client, running_task) = start_request(downloader.clone(), dir.path()).await;
        wait_for(&dir.path().join("download-started")).await;
        drop(running_client);
        running_task.await.unwrap();
        wait_for_cleanup(dir.path()).await;

        std::fs::remove_file(dir.path().join("download-started")).unwrap();
        let (mut unacked_client, unacked_task) =
            start_request(downloader.clone(), dir.path()).await;
        wait_for(&dir.path().join("download-started")).await;
        std::fs::write(dir.path().join("finish"), b"").unwrap();
        let frame = read_frame(&mut unacked_client).await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<Response>(&frame).unwrap(),
            Response::Ok { .. }
        ));
        drop(unacked_client);
        unacked_task.await.unwrap();
        wait_for_cleanup(dir.path()).await;

        let (mut accepted_client, accepted_task) = start_request(downloader, dir.path()).await;
        let frame = read_frame(&mut accepted_client).await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<Response>(&frame).unwrap(),
            Response::Ok { .. }
        ));
        accepted_client.write_u8(1).await.unwrap();
        accepted_task.await.unwrap();
        assert!(!no_artifacts(dir.path()));
    }
}
