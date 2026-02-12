use std::future::Future;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::{
    build_caption, DownloadedItem, DownloadedMedia, Downloader, MediaInfo, MediaType,
};
use crate::telegram_api::TelegramApi;
use crate::validator::validate_media_metadata;

/// An RAII guard to ensure downloaded files are cleaned up.
struct FileCleanupGuard {
    paths: Vec<String>,
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
                    Ok(_) => log::info!("Successfully removed file: {}", path),
                    Err(e) => log::error!("Failed to remove file {}: {}", path, e),
                }
            }
        });
    }
}

/// A helper to execute a Telegram send operation, log the result,
/// and notify the user on failure.
async fn handle_send_operation(
    send_future: impl Future<Output = Result<(), teloxide::RequestError>> + Send,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) {
    match send_future.await {
        Ok(_) => {
            log::info!("Successfully sent to chat_id: {}", chat_id);
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
        }
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
            let error_message = format!(
                "Sorry, I could not fetch information for that link. It might be private or invalid. Error: {}",
                e
            );
            let _ = telegram_api
                .send_text_message(chat_id, message_id, &error_message)
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
            let error_message = format!("Sorry, I could not download the media: {}", e);
            let _ = telegram_api
                .send_text_message(chat_id, message_id, &error_message)
                .await;
            Err(())
        }
    }
}

/// Step 3 (Branch A): Handle sending a single media item.
async fn send_single_item(
    item: &DownloadedItem,
    caption: &str,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) {
    let send_future = match item.media_type {
        MediaType::Video => telegram_api.send_video(
            chat_id,
            message_id,
            &item.filepath,
            caption,
            item.thumbnail_filepath.clone(),
        ),
        MediaType::Photo => telegram_api.send_photo(chat_id, message_id, &item.filepath, caption),
    };
    handle_send_operation(send_future, chat_id, message_id, telegram_api).await;
}

/// Step 3 (Branch B): Handle sending a media group.
async fn send_media_group(
    items: &[DownloadedItem],
    caption: &str,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &dyn TelegramApi,
) {
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
    } else {
        handle_send_operation(
            telegram_api.send_media_group(chat_id, message_id, media_group),
            chat_id,
            message_id,
            telegram_api,
        )
        .await;
    }
}

pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &dyn Downloader,
    telegram_api: &dyn TelegramApi,
) {
    let clean_url = cleanup_url(url);

    let info =
        match pre_download_validation(&clean_url, chat_id, message_id, downloader, telegram_api)
            .await
        {
            Ok(info) => info,
            Err(_) => return,
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
        Err(_) => return,
    };

    let caption = build_caption(&info, &clean_url);
    let _cleanup_guard = FileCleanupGuard::from_downloaded_media(&downloaded);

    match &downloaded {
        DownloadedMedia::Single(item) => {
            send_single_item(item, &caption, chat_id, message_id, telegram_api).await;
        }
        DownloadedMedia::Group(items) => {
            send_media_group(items, &caption, chat_id, message_id, telegram_api).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MockDownloader};
    use crate::telegram_api::MockTelegramApi;
    use crate::test_utils::create_test_info;
    use mockall::predicate::*;
    use teloxide::types::InputMedia;
    use teloxide::types::{ChatId, MessageId};
    use url::Url;

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
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
                    filepath: "/tmp/video.mp4".to_string(),
                    media_type: MediaType::Video,
                    thumbnail_filepath: Some("thumb.jpg".to_string()),
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
                always(),
                eq(Some("thumb.jpg".to_string())),
            )
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_video_without_thumbnail_when_unavailable() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
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
                    filepath: "/tmp/video.mp4".to_string(),
                    media_type: MediaType::Video,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
                always(),
                eq(None),
            )
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_photo_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
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
                    filepath: "/tmp/photo.jpg".to_string(),
                    media_type: MediaType::Photo,
                    thumbnail_filepath: None,
                }))
            });

        mock_telegram_api
            .expect_send_photo()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/photo.jpg"),
                always(),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_media_group_on_multiple_items() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
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
                        filepath: "/tmp/item1.mp4".to_string(),
                        media_type: MediaType::Video,
                        thumbnail_filepath: None,
                    },
                    DownloadedItem {
                        filepath: "/tmp/item2.jpg".to_string(),
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
            .returning(|_, _, _| Ok(()));

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_stops_if_pre_check_fails() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/too_long").unwrap();

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

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_error_on_download_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/invalid_post").unwrap();

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

        process_download_request(
            &test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }
}
