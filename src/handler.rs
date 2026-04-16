use std::path::PathBuf;
use std::time::Instant;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use teloxide::types::InlineKeyboardMarkup;

use crate::downloader::{
    DownloadedItem, DownloadedMedia, Downloader, MediaInfo, MediaType, build_caption,
};
use crate::premium::audio_extractor::AudioExtractor;
use crate::storage::{CachedMedia, Storage};
use crate::telegram_api::{SentMedia, TelegramApi, resize_photo_if_needed};
use crate::validator::validate_media_metadata;

/// Persisted context for a premium action callback button, stored in the DB.
/// Decoupled from subscriptions — tracks the download destination and media info
/// needed to serve the audio/transcribe/summarize callback when the user taps the button.
#[derive(Debug, Clone)]
pub struct CallbackContext {
    pub source_url: String,
    pub chat_id: i64,
    pub has_video: bool,
    pub media_duration_secs: Option<i32>,
    pub audio_cache_path: Option<String>,
    /// Cached raw Deepgram transcript (set after first transcription call).
    pub transcript: Option<String>,
    /// BCP-47 language code detected by Deepgram, e.g. "en", "it".
    pub transcript_language: Option<String>,
}

/// Context returned after a successful download, containing info needed for premium buttons.
pub struct DownloadContext {
    pub source_url: Url,
    pub has_video: bool,
    pub media_duration_secs: Option<i32>,
    pub audio_cache_path: Option<PathBuf>,
    /// Message ID of the sent video, used to attach premium buttons to it.
    pub sent_message_id: Option<MessageId>,
}

/// An RAII guard to ensure downloaded files are cleaned up.
struct FileCleanupGuard {
    paths: Vec<PathBuf>,
}

impl FileCleanupGuard {
    fn from_downloaded_media(media: &DownloadedMedia) -> Self {
        let paths = match media {
            DownloadedMedia::Single(item) => {
                let mut paths = vec![item.filepath.clone()];
                if let Some(thumb) = &item.thumbnail_filepath {
                    paths.push(thumb.clone());
                }
                paths
            }
            DownloadedMedia::Group(items) => {
                items.iter().map(|item| item.filepath.clone()).collect()
            }
        };
        Self { paths }
    }
}

impl Drop for FileCleanupGuard {
    fn drop(&mut self) {
        let paths_to_delete = std::mem::take(&mut self.paths);
        if paths_to_delete.is_empty() {
            return;
        }

        log::info!(
            "Cleanup guard is dropping. Spawning task to delete {} file(s).",
            paths_to_delete.len()
        );

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    for path in &paths_to_delete {
                        match tokio::fs::remove_file(path).await {
                            Ok(_) => log::info!("Successfully removed file: {}", path.display()),
                            Err(e) => {
                                log::error!("Failed to remove file {}: {}", path.display(), e)
                            }
                        }
                    }
                });
            }
            Err(_) => {
                std::thread::spawn(move || {
                    for path in &paths_to_delete {
                        if let Err(e) = std::fs::remove_file(path) {
                            log::error!("Failed to remove file {}: {}", path.display(), e);
                        }
                    }
                });
            }
        }
    }
}

async fn remove_temp_file(path: PathBuf, context: &str) {
    if let Err(e) = tokio::fs::remove_file(&path).await {
        log::warn!(
            "Failed to remove temporary file {} for {}: {}",
            path.display(),
            context,
            e
        );
    }
}

async fn log_reply_failure(
    result: Result<(), teloxide::RequestError>,
    chat_id: ChatId,
    action: &str,
) {
    if let Err(e) = result {
        log::error!(
            "Telegram reply failed: action={} chat_id={} error={:?}",
            action,
            chat_id,
            e
        );
    }
}

/// Creates a normalized URL for use as a cache key:
/// - strips fragment and query params (preserving YouTube `v=` param)
/// - removes `www.` prefix
/// - removes trailing slash from path
#[must_use]
fn cleanup_url(original_url: &Url) -> Url {
    let mut cleaned_url = original_url.clone();
    cleaned_url.set_fragment(None);

    // Normalize www. prefix so e.g. www.instagram.com and instagram.com share a cache entry
    if let Some(host) = cleaned_url.host_str() {
        if let Some(stripped) = host.strip_prefix("www.") {
            let normalized = stripped.to_owned();
            let _ = cleaned_url.set_host(Some(&normalized));
        }
    }

    let is_youtube = cleaned_url
        .host_str()
        .is_some_and(|h| h.ends_with("youtube.com") || h == "youtu.be");

    if is_youtube {
        if let Some(video_id) = original_url
            .query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value)
        {
            cleaned_url.set_query(Some(&format!("v={}", video_id)));
        } else {
            cleaned_url.set_query(None);
        }
    } else {
        cleaned_url.set_query(None);
    }

    // Remove trailing slash from path (e.g. /p/ABC123/ -> /p/ABC123)
    let path = cleaned_url.path().to_owned();
    if path.len() > 1 && path.ends_with('/') {
        let _ = cleaned_url.set_path(path.trim_end_matches('/'));
    }

    cleaned_url
}

/// Step 1: Perform pre-download validation.
async fn pre_download_validation(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &dyn Downloader,
    telegram_api: &dyn TelegramApi,
) -> Result<MediaInfo, ()> {
    log::info!("Beginning pre-download check for {}", url);
    match downloader.get_media_metadata(url).await {
        Ok(info) => {
            if let Err(validation_error) = validate_media_metadata(&info) {
                log::warn!("Validation failed for {}: {}", url, validation_error);
                log_reply_failure(
                    telegram_api
                        .send_text_message(chat_id, message_id, &validation_error.to_string())
                        .await,
                    chat_id,
                    "validation_error",
                )
                .await;
                Err(())
            } else {
                log::info!(
                    "Pre-download checks passed for {}. Proceeding with download.",
                    url
                );
                Ok(info)
            }
        }
        Err(e) => {
            log::error!("Pre-download metadata fetch failed for {}: {}", url, e);
            log_reply_failure(
                telegram_api.send_text_message(
                    chat_id,
                    message_id,
                    "Sorry, I could not fetch information for that link. It might require age verification, be private or unsupported.",
                )
                .await,
                chat_id,
                "metadata_error",
            )
            .await;
            Err(())
        }
    }
}

/// Step 2: Download the media.
async fn download_step(
    info: &MediaInfo,
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &dyn Downloader,
    telegram_api: &dyn TelegramApi,
) -> Result<DownloadedMedia, ()> {
    match downloader.download_media(info, url).await {
        Ok(media) => Ok(media),
        Err(e) => {
            log::error!("Download failed for {}: {}", url, e);
            let user_message = if matches!(e, crate::downloader::DownloadError::Timeout(_)) {
                "Sorry, the download is taking too long. Please try a shorter video."
            } else {
                "Sorry, I could not download the media. Please try again later."
            };
            log_reply_failure(
                telegram_api
                    .send_text_message(chat_id, message_id, user_message)
                    .await,
                chat_id,
                "download_error",
            )
            .await;
            Err(())
        }
    }
}

/// Step 3 (Branch A): Handle sending a single media item. Returns (file_id, media_type, sent_message_id) on success.
async fn send_single_item(
    item: &DownloadedItem,
    caption: &str,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) -> Option<(String, MediaType, MessageId)> {
    let result = match item.media_type {
        MediaType::Video => telegram_api
            .send_video(
                chat_id,
                message_id,
                &item.filepath,
                caption,
                item.thumbnail_filepath.clone(),
            )
            .await
            .map(|(file_id, sent_id)| (file_id, MediaType::Video, sent_id)),
        MediaType::Photo => {
            // Resize happens at the handler layer for both single and group photos.
            let resized = match resize_photo_if_needed(&item.filepath) {
                Ok(resized) => resized,
                Err(e) => {
                    log_reply_failure(
                        telegram_api
                            .send_text_message(chat_id, message_id, &e)
                            .await,
                        chat_id,
                        "photo_policy_reject",
                    )
                    .await;
                    return None;
                }
            };
            let effective_path = resized.as_deref().unwrap_or(&item.filepath);
            let send_result = telegram_api
                .send_photo(chat_id, message_id, effective_path, caption)
                .await
                .map(|(file_id, sent_id)| (file_id, MediaType::Photo, sent_id));
            if let Some(p) = resized {
                remove_temp_file(p, "single photo resize").await;
            }
            send_result
        }
    };

    match result {
        Ok(sent) => {
            log::info!("Successfully sent to chat_id: {}", chat_id);
            Some(sent)
        }
        Err(e) => {
            log::error!("Failed to send: Error: {:?}", e);
            log_reply_failure(
                telegram_api
                    .send_text_message(
                        chat_id,
                        message_id,
                        "Sorry, I encountered an error while sending the media.",
                    )
                    .await,
                chat_id,
                "send_media_error",
            )
            .await;
            None
        }
    }
}

/// Step 3 (Branch B): Handle sending a media group. Returns file_ids on success.
async fn send_media_group_step(
    items: &[DownloadedItem],
    caption: &str,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) -> Option<Vec<SentMedia>> {
    let mut media_group: Vec<InputMedia> = Vec::new();
    let mut temp_resized: Vec<PathBuf> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let item_caption = if i == 0 {
            caption.to_owned()
        } else {
            String::new()
        };

        let media = match item.media_type {
            MediaType::Video => {
                let input_file = InputFile::file(&item.filepath);
                InputMedia::Video(
                    InputMediaVideo::new(input_file)
                        .parse_mode(ParseMode::Html)
                        .caption(item_caption),
                )
            }
            MediaType::Photo => {
                let resized = match resize_photo_if_needed(&item.filepath) {
                    Ok(resized) => resized,
                    Err(e) => {
                        log_reply_failure(
                            telegram_api
                                .send_text_message(chat_id, message_id, &e)
                                .await,
                            chat_id,
                            "photo_policy_reject",
                        )
                        .await;
                        continue;
                    }
                };
                let path = resized.as_deref().unwrap_or(&item.filepath).to_path_buf();
                if let Some(p) = resized {
                    temp_resized.push(p);
                }
                InputMedia::Photo(
                    InputMediaPhoto::new(InputFile::file(path))
                        .parse_mode(ParseMode::Html)
                        .caption(item_caption),
                )
            }
        };
        media_group.push(media);
    }

    if media_group.is_empty() {
        let msg = "Sorry, although multiple items were found, none were of a supported type for a media group.";
        log_reply_failure(
            telegram_api
                .send_text_message(chat_id, message_id, msg)
                .await,
            chat_id,
            "empty_media_group",
        )
        .await;
        return None;
    }

    let result = telegram_api
        .send_media_group(chat_id, message_id, media_group)
        .await;
    for p in temp_resized {
        remove_temp_file(p, "media group resize").await;
    }
    match result {
        Ok(sent) => {
            log::info!("Successfully sent media group to chat_id: {}", chat_id);
            Some(sent)
        }
        Err(e) => {
            log::error!("Failed to send media group: Error: {:?}", e);
            log_reply_failure(
                telegram_api
                    .send_text_message(
                        chat_id,
                        message_id,
                        "Sorry, I encountered an error while sending the media.",
                    )
                    .await,
                chat_id,
                "send_media_group_error",
            )
            .await;
            None
        }
    }
}

/// Send cached media back to the user.
/// Send cached media. For a single video returns `Ok(Some(sent_msg_id))` so the
/// caller can attach premium buttons; all other cases return `Ok(None)`.
async fn send_cached_media(
    cached: &CachedMedia,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) -> Result<Option<MessageId>, ()> {
    if cached.files.len() == 1 {
        let file = &cached.files[0];
        match file.media_type {
            MediaType::Video => {
                match telegram_api
                    .send_cached_video(chat_id, message_id, &file.telegram_file_id, &cached.caption)
                    .await
                {
                    Ok(sent_id) => {
                        log::info!("Successfully sent cached video to chat_id: {}", chat_id);
                        Ok(Some(sent_id))
                    }
                    Err(e) => {
                        log::error!("Failed to send cached video: {:?}", e);
                        Err(())
                    }
                }
            }
            MediaType::Photo => {
                match telegram_api
                    .send_cached_photo(chat_id, message_id, &file.telegram_file_id, &cached.caption)
                    .await
                {
                    Ok(_) => {
                        log::info!("Successfully sent cached photo to chat_id: {}", chat_id);
                        Ok(None)
                    }
                    Err(e) => {
                        log::error!("Failed to send cached photo: {:?}", e);
                        Err(())
                    }
                }
            }
        }
    } else {
        match telegram_api
            .send_cached_media_group(chat_id, message_id, &cached.files, &cached.caption)
            .await
        {
            Ok(_) => {
                log::info!(
                    "Successfully sent cached media group to chat_id: {}",
                    chat_id
                );
                Ok(None)
            }
            Err(e) => {
                log::error!("Failed to send cached media group: {:?}", e);
                Err(())
            }
        }
    }
}

pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &dyn Downloader,
    telegram_api: &dyn TelegramApi,
    storage: &dyn Storage,
    audio_extractor: &dyn AudioExtractor,
) -> Option<DownloadContext> {
    let start = Instant::now();
    let clean_url = cleanup_url(url);
    let clean_url_str = clean_url.as_str();

    // Cache check
    if let Some(cached) = storage.get_cached_media(clean_url_str).await {
        log::info!("Cache hit for {}", clean_url);
        let is_single_video =
            cached.files.len() == 1 && cached.files[0].media_type == MediaType::Video;

        if is_single_video {
            // If we stored an audio path but the file is gone, re-download from scratch.
            let audio_file_missing = cached
                .audio_cache_path
                .as_deref()
                .is_some_and(|p| !std::path::Path::new(p).exists());
            if audio_file_missing {
                log::warn!(
                    "Cached audio file missing for {}, falling through to re-download",
                    clean_url
                );
            } else if let Ok(sent_message_id) =
                send_cached_media(&cached, chat_id, message_id, telegram_api).await
            {
                storage
                    .log_request(
                        chat_id.0,
                        clean_url_str,
                        "cached",
                        start.elapsed().as_millis() as i64,
                    )
                    .await;
                return Some(DownloadContext {
                    source_url: clean_url,
                    has_video: true,
                    media_duration_secs: cached.media_duration_secs,
                    audio_cache_path: cached.audio_cache_path.map(PathBuf::from),
                    sent_message_id,
                });
            }
        } else if send_cached_media(&cached, chat_id, message_id, telegram_api)
            .await
            .is_ok()
        {
            storage
                .log_request(
                    chat_id.0,
                    clean_url_str,
                    "cached",
                    start.elapsed().as_millis() as i64,
                )
                .await;
            return None;
        }
        // Cache send failed — fall through to normal download
        log::warn!(
            "Cache send failed for {}, falling through to download",
            clean_url
        );
    }

    let info =
        match pre_download_validation(&clean_url, chat_id, message_id, downloader, telegram_api)
            .await
        {
            Ok(info) => info,
            Err(_) => {
                storage
                    .log_request(
                        chat_id.0,
                        clean_url_str,
                        "validation_error",
                        start.elapsed().as_millis() as i64,
                    )
                    .await;
                return None;
            }
        };

    let downloaded = match download_step(
        &info,
        &clean_url,
        chat_id,
        message_id,
        downloader,
        telegram_api,
    )
    .await
    {
        Ok(media) => media,
        Err(_) => {
            storage
                .log_request(
                    chat_id.0,
                    clean_url_str,
                    "error",
                    start.elapsed().as_millis() as i64,
                )
                .await;
            return None;
        }
    };

    let caption = build_caption(&info, &clean_url);
    let _cleanup_guard = FileCleanupGuard::from_downloaded_media(&downloaded);

    // For a single video item, run upload and audio extraction concurrently.
    // For groups or photos, just upload normally (no audio extraction).
    let (file_ids, audio_cache_path, media_duration_secs, has_video, sent_message_id) =
        match &downloaded {
            DownloadedMedia::Single(item) if item.media_type == MediaType::Video => {
                let (send_result, audio_result) = tokio::join!(
                    send_single_item(item, &caption, chat_id, message_id, telegram_api),
                    audio_extractor.extract_audio(
                        &item.filepath,
                        info.title.clone(),
                        info.uploader.clone()
                    )
                );
                let (file_ids, sent_msg_id) = match send_result {
                    Some((file_id, media_type, msg_id)) => {
                        (Some(vec![(file_id, media_type)]), Some(msg_id))
                    }
                    None => (None, None),
                };
                let (audio_cache_path, media_duration_secs) = match audio_result {
                    Ok(result) => (Some(result.audio_path), Some(result.duration_secs)),
                    Err(e) => {
                        log::warn!("Audio extraction failed: {}", e);
                        (None, None)
                    }
                };
                (
                    file_ids,
                    audio_cache_path,
                    media_duration_secs,
                    true,
                    sent_msg_id,
                )
            }
            DownloadedMedia::Single(item) => {
                let (file_ids, sent_msg_id) =
                    match send_single_item(item, &caption, chat_id, message_id, telegram_api).await
                    {
                        Some((file_id, media_type, msg_id)) => {
                            (Some(vec![(file_id, media_type)]), Some(msg_id))
                        }
                        None => (None, None),
                    };
                (file_ids, None, None, false, sent_msg_id)
            }
            DownloadedMedia::Group(items) => {
                let file_ids =
                    send_media_group_step(items, &caption, chat_id, message_id, telegram_api)
                        .await
                        .map(|sent| {
                            sent.into_iter()
                                .map(|s| (s.file_id, s.media_type))
                                .collect()
                        });
                (file_ids, None, None, false, None)
            }
        };

    let elapsed_ms = start.elapsed().as_millis() as i64;

    if let Some(files) = &file_ids {
        if has_video && audio_cache_path.is_none() {
            log_reply_failure(
                telegram_api.send_text_message(
                    chat_id,
                    message_id,
                    "Audio extraction failed — AI features (Extract Audio, Transcribe, Summarize) are not available for this video.",
                )
                .await,
                chat_id,
                "audio_extraction_notice",
            )
            .await;
        }
        storage
            .store_cached_media(
                clean_url_str,
                &caption,
                files,
                audio_cache_path
                    .as_deref()
                    .and_then(|p| p.to_str())
                    .map(String::from),
                media_duration_secs,
            )
            .await;
        storage
            .log_request(chat_id.0, clean_url_str, "success", elapsed_ms)
            .await;
        Some(DownloadContext {
            source_url: clean_url,
            has_video,
            media_duration_secs,
            audio_cache_path,
            sent_message_id,
        })
    } else {
        storage
            .log_request(chat_id.0, clean_url_str, "error", elapsed_ms)
            .await;
        None
    }
}

/// Split long text into multiple messages (Telegram max ~4000 chars per message).
pub async fn send_long_text(
    chat_id: ChatId,
    message_id: MessageId,
    text: &str,
    api: &dyn TelegramApi,
) {
    const MAX_LEN: usize = 4000;
    if text.len() <= MAX_LEN {
        log_reply_failure(
            api.send_text_message(chat_id, message_id, text).await,
            chat_id,
            "long_text_chunk",
        )
        .await;
        return;
    }
    let mut start = 0;
    while start < text.len() {
        let end = text.floor_char_boundary((start + MAX_LEN).min(text.len()));
        let chunk = &text[start..end];
        log_reply_failure(
            api.send_text_message(chat_id, message_id, chunk).await,
            chat_id,
            "long_text_chunk",
        )
        .await;
        start = end;
    }
}

/// Store a callback context and attach premium action buttons to the sent video message.
pub async fn maybe_send_premium_buttons(
    chat_id: ChatId,
    ctx: DownloadContext,
    api: &dyn TelegramApi,
    storage: &dyn Storage,
) {
    if !ctx.has_video || ctx.audio_cache_path.is_none() {
        return;
    }

    let sent_msg_id = match ctx.sent_message_id {
        Some(id) => id,
        None => {
            log::warn!("No sent_message_id for premium buttons, skipping");
            return;
        }
    };

    let callback_ctx = CallbackContext {
        source_url: ctx.source_url.to_string(),
        chat_id: chat_id.0,
        has_video: ctx.has_video,
        media_duration_secs: ctx.media_duration_secs,
        audio_cache_path: ctx
            .audio_cache_path
            .map(|p| p.to_string_lossy().to_string()),
        transcript: None,
        transcript_language: None,
    };

    let context_id = storage.store_callback_context(&callback_ctx).await;
    if context_id == 0 {
        log::warn!("Failed to store callback context, skipping premium buttons");
        return;
    }

    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        teloxide::types::InlineKeyboardButton::callback(
            "Extract Audio",
            format!("audio:{}", context_id),
        ),
        teloxide::types::InlineKeyboardButton::callback(
            "Transcribe",
            format!("txn:{}", context_id),
        ),
        teloxide::types::InlineKeyboardButton::callback("Summarize", format!("sum:{}", context_id)),
    ]]);

    if let Err(e) = api
        .edit_message_reply_markup(chat_id, sent_msg_id, keyboard)
        .await
    {
        log::warn!("Failed to attach premium buttons to video: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MockDownloader};
    use crate::premium::audio_extractor::{AudioExtractionResult, MockAudioExtractor};
    use crate::storage::MockStorage;
    use crate::telegram_api::{MockTelegramApi, SentMedia};
    use crate::test_utils::create_test_info;
    use mockall::predicate::*;
    use std::path::Path;
    use teloxide::types::InputMedia;
    use teloxide::types::{ChatId, MessageId};
    use url::Url;

    /// Helper to create a MockStorage that returns no cache and expects log_request.
    fn create_default_mock_storage() -> MockStorage {
        let mut mock_storage = MockStorage::new();
        mock_storage.expect_get_cached_media().returning(|_| None);
        mock_storage
            .expect_store_cached_media()
            .returning(|_, _, _, _, _: Option<i32>| ());
        mock_storage.expect_log_request().returning(|_, _, _, _| ());
        mock_storage
    }

    /// Helper to create a MockAudioExtractor that fails (non-fatal).
    fn create_failing_audio_extractor() -> MockAudioExtractor {
        let mut mock = MockAudioExtractor::new();
        mock.expect_extract_audio().returning(|_, _, _| {
            Err(
                crate::premium::audio_extractor::AudioExtractionError::FfmpegError(
                    "not available in test".to_string(),
                ),
            )
        });
        mock
    }

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/valid_post").unwrap();

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .withf(|info, url| {
                info.id == "123" && url.as_str() == "https://instagram.com/p/valid_post"
            })
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/video.mp4"),
                    media_type: MediaType::Video,
                    thumbnail_filepath: Some(PathBuf::from("thumb.jpg")),
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq(Path::new("/tmp/video.mp4")),
                always(),
                eq(Some(PathBuf::from("thumb.jpg"))),
            )
            .times(1)
            .returning(|_, _, _, _, _| Ok(("file_id_video_123".to_string(), MessageId(0))));

        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_video_without_thumbnail_when_unavailable() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/valid_post_no_thumb").unwrap();

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .withf(|info, _url| info.id == "123")
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/video.mp4"),
                    media_type: MediaType::Video,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq(Path::new("/tmp/video.mp4")),
                always(),
                eq(None::<PathBuf>),
            )
            .times(1)
            .returning(|_, _, _, _, _| Ok(("file_id_video_456".to_string(), MessageId(0))));

        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_photo_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/valid_photo").unwrap();

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .withf(|info, _url| info.id == "123")
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/photo.jpg"),
                    media_type: MediaType::Photo,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_photo()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq(Path::new("/tmp/photo.jpg")),
                always(),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(("file_id_photo_123".to_string(), MessageId(0))));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_media_group_on_multiple_items() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/multiple_media").unwrap();

        let mut pre_download_info = create_test_info();
        pre_download_info.entries = Some(vec![create_test_info(), create_test_info()]);

        let info_for_get = pre_download_info.clone();
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| Ok(info_for_get.clone()));

        mock_downloader
            .expect_download_media()
            .withf(|info, _url| info.entries.is_some())
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Group(vec![
                    DownloadedItem {
                        filepath: PathBuf::from("/tmp/item1.mp4"),
                        media_type: MediaType::Video,
                        thumbnail_filepath: None,
                    },
                    DownloadedItem {
                        filepath: PathBuf::from("/tmp/item2.jpg"),
                        media_type: MediaType::Photo,
                        thumbnail_filepath: None,
                    },
                ]))
            });

        mock_telegram_api
            .expect_send_media_group()
            .withf(|_, _, media_vec: &Vec<InputMedia>| {
                media_vec.len() == 2
                    && matches!(&media_vec[0], InputMedia::Video(v) if v.caption.as_ref().is_some_and(|c| !c.is_empty()))
                    && matches!(&media_vec[1], InputMedia::Photo(p) if p.caption.as_ref().is_some_and(|c| c.is_empty()))
            })
            .times(1)
            .returning(|_, _, _| {
                Ok(vec![
                    SentMedia {
                        file_id: "file_id_group_1".to_string(),
                        media_type: MediaType::Video,
                    },
                    SentMedia {
                        file_id: "file_id_group_2".to_string(),
                        media_type: MediaType::Photo,
                    },
                ])
            });

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_stops_if_pre_check_fails() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/too_long").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| {
                let mut info = create_test_info();
                info.duration = Some(9999.0);
                Ok(info)
            });

        mock_downloader.expect_download_media().times(0);

        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| msg.contains("too long"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "validation_error")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_error_on_download_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/invalid_post").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .withf(|info, _url| info.id == "123")
            .times(1)
            .returning(|_, _| Err(DownloadError::CommandFailed("yt-dlp exploded".to_string())));

        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| msg.contains("could not download the media"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        mock_telegram_api.expect_send_video().times(0);
        mock_telegram_api.expect_send_photo().times(0);
        mock_telegram_api.expect_send_media_group().times(0);

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "error")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_timeout_message_on_timeout() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/slow_video").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .withf(|info, _url| info.id == "123")
            .times(1)
            .returning(|_, _| Err(DownloadError::Timeout(300)));

        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| msg.contains("taking too long"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        mock_telegram_api.expect_send_video().times(0);
        mock_telegram_api.expect_send_photo().times(0);
        mock_telegram_api.expect_send_media_group().times(0);

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "error")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_generic_error_on_metadata_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/private_post").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| {
                Err(DownloadError::CommandFailed(
                    "ERROR: /usr/local/bin/yt-dlp: private video".to_string(),
                ))
            });

        mock_downloader.expect_download_media().times(0);

        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| {
                msg.contains("could not fetch information")
                    && !msg.contains("ERROR:")
                    && !msg.contains("yt-dlp")
            })
            .times(1)
            .returning(|_, _, _| Ok(()));

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "validation_error")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_send_failure_falls_through_to_download() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/stale_cache").unwrap();

        // Cache returns data but send fails (e.g. stale file_id)
        mock_storage.expect_get_cached_media().returning(|_| {
            Some(CachedMedia {
                caption: "old caption".to_string(),
                files: vec![crate::storage::CachedFile {
                    telegram_file_id: "stale_file_id".to_string(),
                    media_type: MediaType::Video,
                }],
                audio_cache_path: None,
                media_duration_secs: None,
            })
        });

        mock_telegram_api
            .expect_send_cached_video()
            .times(1)
            .returning(|_, _, _, _| {
                Err(teloxide::RequestError::Api(teloxide::ApiError::Unknown(
                    "Bad Request: wrong file_id".to_string(),
                )))
            });

        // Falls through to normal download pipeline
        mock_downloader
            .expect_get_media_metadata()
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/video.mp4"),
                    media_type: MediaType::Video,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .times(1)
            .returning(|_, _, _, _, _| Ok(("fresh_file_id".to_string(), MessageId(0))));

        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));

        mock_storage
            .expect_store_cached_media()
            .times(1)
            .returning(|_, _, _, _, _: Option<i32>| ());

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "success")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_send_failure_after_download_logs_error() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/send_fail").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .returning(|_| Ok(create_test_info()));

        mock_downloader.expect_download_media().returning(|_, _| {
            Ok(DownloadedMedia::Single(DownloadedItem {
                filepath: PathBuf::from("/tmp/video.mp4"),
                media_type: MediaType::Video,
                thumbnail_filepath: None,
            }))
        });

        mock_telegram_api
            .expect_send_video()
            .times(1)
            .returning(|_, _, _, _, _| {
                Err(teloxide::RequestError::Api(teloxide::ApiError::Unknown(
                    "Request Entity Too Large".to_string(),
                )))
            });

        // send_single_item sends error text on failure
        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));

        // No cache store when send fails
        mock_storage.expect_store_cached_media().times(0);

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "error")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_hit_sends_cached_video_without_download() {
        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_post").unwrap();

        mock_storage
            .expect_get_cached_media()
            .with(eq("https://instagram.com/p/cached_post"))
            .times(1)
            .returning(|_| {
                Some(CachedMedia {
                    caption: "cached caption".to_string(),
                    files: vec![crate::storage::CachedFile {
                        telegram_file_id: "cached_file_id".to_string(),
                        media_type: MediaType::Video,
                    }],
                    audio_cache_path: None,
                    media_duration_secs: None,
                })
            });

        mock_telegram_api
            .expect_send_cached_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("cached_file_id"),
                eq("cached caption"),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(MessageId(789)));

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "cached")
            .times(1)
            .returning(|_, _, _, _| ());

        // Audio extraction runs concurrently; failing is non-fatal
        let ctx = process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;

        // Even with failed audio extraction we get a DownloadContext for the video
        let ctx = ctx.expect("expected Some(DownloadContext) for cached video");
        assert!(ctx.has_video);
        assert!(ctx.audio_cache_path.is_none()); // audio failed
        assert_eq!(ctx.sent_message_id, Some(MessageId(789)));
    }

    #[tokio::test]
    async fn test_cache_hit_video_with_stored_audio_returns_download_context() {
        // Simulate a cache hit where audio_cache_path was persisted in the DB.
        // The test uses a path that "exists" — we pass a path to a real temp file.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"fake mp3 data").unwrap();
        let audio_path = tmp.path().to_str().unwrap().to_string();

        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_video").unwrap();

        mock_storage
            .expect_get_cached_media()
            .times(1)
            .returning(move |_| {
                Some(CachedMedia {
                    caption: "video caption".to_string(),
                    files: vec![crate::storage::CachedFile {
                        telegram_file_id: "cached_video_id".to_string(),
                        media_type: MediaType::Video,
                    }],
                    audio_cache_path: Some(audio_path.clone()),
                    media_duration_secs: Some(120),
                })
            });

        mock_telegram_api
            .expect_send_cached_video()
            .times(1)
            .returning(|_, _, _, _| Ok(MessageId(101)));

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "cached")
            .times(1)
            .returning(|_, _, _, _| ());

        let ctx = process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;

        let ctx = ctx.expect("expected Some(DownloadContext) for cached video with audio");
        assert!(ctx.has_video);
        assert!(ctx.audio_cache_path.is_some());
        assert_eq!(ctx.media_duration_secs, Some(120));
        assert_eq!(ctx.sent_message_id, Some(MessageId(101)));
    }

    #[tokio::test]
    async fn test_cache_hit_video_missing_audio_file_falls_through_to_download() {
        // If the DB has an audio path but the file is gone, we should re-download
        // the video from scratch rather than serving a degraded cached version.
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_video").unwrap();

        mock_storage
            .expect_get_cached_media()
            .times(1)
            .returning(|_| {
                Some(CachedMedia {
                    caption: "video caption".to_string(),
                    files: vec![crate::storage::CachedFile {
                        telegram_file_id: "cached_video_id".to_string(),
                        media_type: MediaType::Video,
                    }],
                    // Path that does not exist on disk
                    audio_cache_path: Some("/tmp/audio_cache/gone.mp3".to_string()),
                    media_duration_secs: Some(120),
                })
            });

        // send_cached_video must NOT be called — we fall through to fresh download
        mock_telegram_api.expect_send_cached_video().times(0);

        // Falls through to normal download pipeline
        mock_downloader
            .expect_get_media_metadata()
            .times(1)
            .returning(|_| Ok(create_test_info()));
        mock_downloader
            .expect_download_media()
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/video.mp4"),
                    media_type: MediaType::Video,
                    thumbnail_filepath: None,
                }))
            });
        mock_telegram_api
            .expect_send_video()
            .times(1)
            .returning(|_, _, _, _, _| Ok(("fresh_file_id".to_string(), MessageId(0))));
        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));
        mock_storage
            .expect_store_cached_media()
            .times(1)
            .returning(|_, _, _, _, _| ());
        mock_storage
            .expect_log_request()
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_hit_sends_cached_photo() {
        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_photo").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| {
            Some(CachedMedia {
                caption: "photo caption".to_string(),
                files: vec![crate::storage::CachedFile {
                    telegram_file_id: "cached_photo_id".to_string(),
                    media_type: MediaType::Photo,
                }],
                audio_cache_path: None,
                media_duration_secs: None,
            })
        });

        mock_telegram_api
            .expect_send_cached_photo()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("cached_photo_id"),
                eq("photo caption"),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        mock_storage.expect_log_request().returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_hit_sends_cached_media_group() {
        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_group").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| {
            Some(CachedMedia {
                caption: "group caption".to_string(),
                files: vec![
                    crate::storage::CachedFile {
                        telegram_file_id: "file_1".to_string(),
                        media_type: MediaType::Video,
                    },
                    crate::storage::CachedFile {
                        telegram_file_id: "file_2".to_string(),
                        media_type: MediaType::Photo,
                    },
                ],
                audio_cache_path: None,
                media_duration_secs: None,
            })
        });

        mock_telegram_api
            .expect_send_cached_media_group()
            .withf(|_, _, files, caption| files.len() == 2 && caption == "group caption")
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        mock_storage.expect_log_request().returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_miss_downloads_and_stores() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/new_post").unwrap();

        mock_storage.expect_get_cached_media().returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .returning(|_| Ok(create_test_info()));

        mock_downloader.expect_download_media().returning(|_, _| {
            Ok(DownloadedMedia::Single(DownloadedItem {
                filepath: PathBuf::from("/tmp/video.mp4"),
                media_type: MediaType::Video,
                thumbnail_filepath: None,
            }))
        });

        mock_telegram_api
            .expect_send_video()
            .times(1)
            .returning(|_, _, _, _, _| Ok(("new_file_id".to_string(), MessageId(0))));

        mock_telegram_api
            .expect_send_text_message()
            .returning(|_, _, _| Ok(()));

        mock_storage
            .expect_store_cached_media()
            .withf(|url, _caption, files, _audio, _dur| {
                url == "https://instagram.com/p/new_post"
                    && files.len() == 1
                    && files[0].0 == "new_file_id"
            })
            .times(1)
            .returning(|_, _, _, _, _| ());

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "success")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_returns_audio_context_on_extraction_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/valid_post").unwrap();

        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
            .times(1)
            .returning(|_, _| {
                Ok(DownloadedMedia::Single(DownloadedItem {
                    filepath: PathBuf::from("/tmp/video.mp4"),
                    media_type: MediaType::Video,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .times(1)
            .returning(|_, _, _, _, _| Ok(("file_id_123".to_string(), MessageId(0))));

        let mut mock_audio = MockAudioExtractor::new();
        mock_audio.expect_extract_audio().returning(|_, _, _| {
            Ok(AudioExtractionResult {
                audio_path: PathBuf::from("/tmp/audio_cache/test.mp3"),
                duration_secs: 42,
            })
        });

        let ctx = process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &mock_audio,
        )
        .await
        .expect("expected Some(DownloadContext)");

        assert!(ctx.has_video);
        assert_eq!(
            ctx.audio_cache_path,
            Some(PathBuf::from("/tmp/audio_cache/test.mp3"))
        );
        assert_eq!(ctx.media_duration_secs, Some(42));
    }

    #[tokio::test]
    async fn test_process_download_request_photo_returns_no_video_context() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mock_storage = create_default_mock_storage();
        let test_url = Url::parse("https://instagram.com/p/photo_post").unwrap();

        mock_downloader
            .expect_get_media_metadata()
            .returning(|_| Ok(create_test_info()));

        mock_downloader.expect_download_media().returning(|_, _| {
            Ok(DownloadedMedia::Single(DownloadedItem {
                filepath: PathBuf::from("/tmp/photo.jpg"),
                media_type: MediaType::Photo,
                thumbnail_filepath: None,
            }))
        });

        mock_telegram_api
            .expect_send_photo()
            .times(1)
            .returning(|_, _, _, _| Ok(("photo_file_id".to_string(), MessageId(0))));

        let ctx = process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
            &create_failing_audio_extractor(),
        )
        .await
        .expect("expected Some(DownloadContext)");

        assert!(!ctx.has_video);
        assert!(ctx.audio_cache_path.is_none());
        assert!(ctx.media_duration_secs.is_none());
    }

    // ── send_long_text ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_send_long_text_short_sends_single_message() {
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_send_text_message()
            .with(eq(ChatId(1)), eq(MessageId(1)), eq("hello"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        send_long_text(ChatId(1), MessageId(1), "hello", &mock_api).await;
    }

    #[tokio::test]
    async fn test_send_long_text_exactly_at_limit_is_single_message() {
        let text = "x".repeat(4000);
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        send_long_text(ChatId(1), MessageId(1), &text, &mock_api).await;
    }

    #[tokio::test]
    async fn test_send_long_text_over_limit_splits_into_two_messages() {
        let text = "y".repeat(4001);
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_send_text_message()
            .times(2)
            .returning(|_, _, _| Ok(()));

        send_long_text(ChatId(1), MessageId(1), &text, &mock_api).await;
    }

    #[tokio::test]
    async fn test_send_long_text_large_sends_correct_number_of_chunks() {
        let text = "z".repeat(4000 * 3 + 1); // 4 chunks
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_send_text_message()
            .times(4)
            .returning(|_, _, _| Ok(()));

        send_long_text(ChatId(1), MessageId(1), &text, &mock_api).await;
    }

    // ── maybe_send_premium_buttons ────────────────────────────────────

    fn make_download_ctx(has_video: bool, audio_cache_path: Option<PathBuf>) -> DownloadContext {
        DownloadContext {
            source_url: "https://example.com/video".parse().unwrap(),
            has_video,
            media_duration_secs: audio_cache_path.as_ref().map(|_| 60),
            sent_message_id: if has_video { Some(MessageId(99)) } else { None },
            audio_cache_path,
        }
    }

    #[tokio::test]
    async fn test_maybe_send_premium_buttons_no_video_is_noop() {
        let api = MockTelegramApi::new();
        let storage = MockStorage::new();
        let ctx = make_download_ctx(false, Some(PathBuf::from("/tmp/audio.mp3")));

        maybe_send_premium_buttons(ChatId(1), ctx, &api, &storage).await;
    }

    #[tokio::test]
    async fn test_maybe_send_premium_buttons_no_audio_cache_is_noop() {
        let api = MockTelegramApi::new();
        let storage = MockStorage::new();
        let ctx = make_download_ctx(true, None);

        maybe_send_premium_buttons(ChatId(1), ctx, &api, &storage).await;
    }

    #[tokio::test]
    async fn test_maybe_send_premium_buttons_store_failure_skips_keyboard() {
        let api = MockTelegramApi::new();
        let mut storage = MockStorage::new();
        storage
            .expect_store_callback_context()
            .times(1)
            .returning(|_| 0);

        let ctx = make_download_ctx(true, Some(PathBuf::from("/tmp/audio.mp3")));
        maybe_send_premium_buttons(ChatId(1), ctx, &api, &storage).await;
    }

    #[tokio::test]
    async fn test_maybe_send_premium_buttons_success_sends_keyboard() {
        let mut storage = MockStorage::new();
        storage
            .expect_store_callback_context()
            .times(1)
            .returning(|_| 42);

        let mut api = MockTelegramApi::new();
        api.expect_edit_message_reply_markup()
            .withf(|chat_id, msg_id, keyboard| {
                chat_id.0 == 1
                    && msg_id.0 == 99
                    && keyboard
                        .inline_keyboard
                        .iter()
                        .flatten()
                        .any(|b| b.text == "Extract Audio")
                    && keyboard
                        .inline_keyboard
                        .iter()
                        .flatten()
                        .any(|b| b.text == "Transcribe")
                    && keyboard
                        .inline_keyboard
                        .iter()
                        .flatten()
                        .any(|b| b.text == "Summarize")
            })
            .times(1)
            .returning(|_, _, _| Ok(()));

        let ctx = make_download_ctx(true, Some(PathBuf::from("/tmp/audio.mp3")));
        maybe_send_premium_buttons(ChatId(1), ctx, &api, &storage).await;
    }
}
