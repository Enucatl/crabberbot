use std::path::PathBuf;
use std::time::Instant;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::{
    build_caption, DownloadedItem, DownloadedMedia, Downloader, MediaInfo, MediaType,
};
use crate::storage::{CachedMedia, Storage};
use crate::telegram_api::{SentMedia, TelegramApi};
use crate::validator::validate_media_metadata;

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

        tokio::spawn(async move {
            for path in &paths_to_delete {
                match tokio::fs::remove_file(path).await {
                    Ok(_) => log::info!("Successfully removed file: {}", path.display()),
                    Err(e) => log::error!("Failed to remove file {}: {}", path.display(), e),
                }
            }
        });
    }
}

/// Creates a new URL with the query string and fragment removed.
fn cleanup_url(original_url: &Url) -> Url {
    let mut cleaned_url = original_url.clone();
    cleaned_url.set_fragment(None);

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
                let _ = telegram_api
                    .send_text_message(chat_id, message_id, &validation_error.to_string())
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
            let _ = telegram_api
                .send_text_message(
                    chat_id,
                    message_id,
                    "Sorry, I could not fetch information for that link. It might require age verification, be private or unsupported.",
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
            let _ = telegram_api
                .send_text_message(chat_id, message_id, user_message)
                .await;
            Err(())
        }
    }
}

/// Step 3 (Branch A): Handle sending a single media item. Returns file_id on success.
async fn send_single_item(
    item: &DownloadedItem,
    caption: &str,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) -> Option<(String, MediaType)> {
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
            .map(|file_id| (file_id, MediaType::Video)),
        MediaType::Photo => telegram_api
            .send_photo(chat_id, message_id, &item.filepath, caption)
            .await
            .map(|file_id| (file_id, MediaType::Photo)),
    };

    match result {
        Ok(sent) => {
            log::info!("Successfully sent to chat_id: {}", chat_id);
            Some(sent)
        }
        Err(e) => {
            log::error!("Failed to send: Error: {:?}", e);
            let _ = telegram_api
                .send_text_message(
                    chat_id,
                    message_id,
                    "Sorry, I encountered an error while sending the media.",
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

    for (i, item) in items.iter().enumerate() {
        let input_file = InputFile::file(&item.filepath);
        let item_caption = if i == 0 {
            caption.to_string()
        } else {
            String::new()
        };

        let media = match item.media_type {
            MediaType::Video => InputMedia::Video(
                InputMediaVideo::new(input_file)
                    .parse_mode(ParseMode::Html)
                    .caption(item_caption),
            ),
            MediaType::Photo => InputMedia::Photo(
                InputMediaPhoto::new(input_file)
                    .parse_mode(ParseMode::Html)
                    .caption(item_caption),
            ),
        };
        media_group.push(media);
    }

    if media_group.is_empty() {
        let msg = "Sorry, although multiple items were found, none were of a supported type for a media group.";
        let _ = telegram_api
            .send_text_message(chat_id, message_id, msg)
            .await;
        return None;
    }

    match telegram_api
        .send_media_group(chat_id, message_id, media_group)
        .await
    {
        Ok(sent) => {
            log::info!("Successfully sent media group to chat_id: {}", chat_id);
            Some(sent)
        }
        Err(e) => {
            log::error!("Failed to send media group: Error: {:?}", e);
            let _ = telegram_api
                .send_text_message(
                    chat_id,
                    message_id,
                    "Sorry, I encountered an error while sending the media.",
                )
                .await;
            None
        }
    }
}

/// Send cached media back to the user.
async fn send_cached_media(
    cached: &CachedMedia,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) -> Result<(), ()> {
    let result = if cached.files.len() == 1 {
        let file = &cached.files[0];
        match file.media_type {
            MediaType::Video => telegram_api
                .send_cached_video(chat_id, message_id, &file.telegram_file_id, &cached.caption)
                .await,
            MediaType::Photo => telegram_api
                .send_cached_photo(chat_id, message_id, &file.telegram_file_id, &cached.caption)
                .await,
        }
    } else {
        telegram_api
            .send_cached_media_group(chat_id, message_id, &cached.files, &cached.caption)
            .await
    };

    match result {
        Ok(_) => {
            log::info!("Successfully sent cached media to chat_id: {}", chat_id);
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to send cached media: {:?}", e);
            Err(())
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
) {
    let start = Instant::now();
    let clean_url = cleanup_url(url);
    let clean_url_str = clean_url.as_str();

    // Cache check
    if let Some(cached) = storage.get_cached_media(clean_url_str).await {
        log::info!("Cache hit for {}", clean_url);
        if send_cached_media(&cached, chat_id, message_id, telegram_api)
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
            return;
        }
        // Cache send failed — fall through to normal download
        log::warn!("Cache send failed for {}, falling through to download", clean_url);
    }

    let info = match pre_download_validation(
        &clean_url,
        chat_id,
        message_id,
        downloader,
        telegram_api,
    )
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
            return;
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
            return;
        }
    };

    let caption = build_caption(&info, &clean_url);
    let _cleanup_guard = FileCleanupGuard::from_downloaded_media(&downloaded);

    let file_ids: Option<Vec<(String, MediaType)>> = match &downloaded {
        DownloadedMedia::Single(item) => {
            send_single_item(item, &caption, chat_id, message_id, telegram_api)
                .await
                .map(|sent| vec![sent])
        }
        DownloadedMedia::Group(items) => {
            send_media_group_step(items, &caption, chat_id, message_id, telegram_api)
                .await
                .map(|sent| {
                    sent.into_iter()
                        .map(|s| (s.file_id, s.media_type))
                        .collect()
                })
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as i64;

    if let Some(files) = &file_ids {
        storage
            .store_cached_media(clean_url_str, &caption, files)
            .await;
        storage
            .log_request(chat_id.0, clean_url_str, "success", elapsed_ms)
            .await;
    } else {
        storage
            .log_request(chat_id.0, clean_url_str, "error", elapsed_ms)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MockDownloader};
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
        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);
        mock_storage
            .expect_store_cached_media()
            .returning(|_, _, _| ());
        mock_storage
            .expect_log_request()
            .returning(|_, _, _, _| ());
        mock_storage
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
            .returning(|_, _, _, _, _| Ok("file_id_video_123".to_string()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
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
            .returning(|_, _, _, _, _| Ok("file_id_video_456".to_string()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
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
            .returning(|_, _, _, _| Ok("file_id_photo_123".to_string()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
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
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_stops_if_pre_check_fails() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/too_long").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

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
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_error_on_download_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/invalid_post").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

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
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_timeout_message_on_timeout() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/slow_video").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

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
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_generic_error_on_metadata_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/private_post").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

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
        mock_storage
            .expect_get_cached_media()
            .returning(|_| {
                Some(CachedMedia {
                    caption: "old caption".to_string(),
                    files: vec![crate::storage::CachedFile {
                        telegram_file_id: "stale_file_id".to_string(),
                        media_type: MediaType::Video,
                    }],
                })
            });

        mock_telegram_api
            .expect_send_cached_video()
            .times(1)
            .returning(|_, _, _, _| {
                Err(teloxide::RequestError::Api(
                    teloxide::ApiError::Unknown("Bad Request: wrong file_id".to_string()),
                ))
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
            .returning(|_, _, _, _, _| Ok("fresh_file_id".to_string()));

        mock_storage
            .expect_store_cached_media()
            .times(1)
            .returning(|_, _, _| ());

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
        )
        .await;
    }

    #[tokio::test]
    async fn test_send_failure_after_download_logs_error() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/send_fail").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
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
            .returning(|_, _, _, _, _| {
                Err(teloxide::RequestError::Api(
                    teloxide::ApiError::Unknown("Request Entity Too Large".to_string()),
                ))
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
            .returning(|_, _, _, _| Ok(()));

        mock_storage
            .expect_log_request()
            .withf(|_, _, status, _| status == "cached")
            .times(1)
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_hit_sends_cached_photo() {
        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_photo").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| {
                Some(CachedMedia {
                    caption: "photo caption".to_string(),
                    files: vec![crate::storage::CachedFile {
                        telegram_file_id: "cached_photo_id".to_string(),
                        media_type: MediaType::Photo,
                    }],
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

        mock_storage
            .expect_log_request()
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_hit_sends_cached_media_group() {
        let mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/cached_group").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| {
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
                })
            });

        mock_telegram_api
            .expect_send_cached_media_group()
            .withf(|_, _, files, caption| {
                files.len() == 2 && caption == "group caption"
            })
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        mock_storage
            .expect_log_request()
            .returning(|_, _, _, _| ());

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
            &mock_storage,
        )
        .await;
    }

    #[tokio::test]
    async fn test_cache_miss_downloads_and_stores() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let test_url = Url::parse("https://instagram.com/p/new_post").unwrap();

        mock_storage
            .expect_get_cached_media()
            .returning(|_| None);

        mock_downloader
            .expect_get_media_metadata()
            .returning(|_| Ok(create_test_info()));

        mock_downloader
            .expect_download_media()
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
            .returning(|_, _, _, _, _| Ok("new_file_id".to_string()));

        mock_storage
            .expect_store_cached_media()
            .withf(|url, _caption, files| {
                url == "https://instagram.com/p/new_post"
                    && files.len() == 1
                    && files[0].0 == "new_file_id"
            })
            .times(1)
            .returning(|_, _, _| ());

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
        )
        .await;
    }
}
