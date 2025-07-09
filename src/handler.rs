use std::future::Future;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::{Downloader, MediaMetadata};
use crate::telegram_api::TelegramApi;
use crate::validator::validate_media_metadata;

/// An RAII guard to ensure downloaded files are cleaned up.
/// When this struct goes out of scope, its `drop` implementation
/// is called, which spawns a task to delete the files.
struct FileCleanupGuard {
    paths: Vec<String>,
}

impl Drop for FileCleanupGuard {
    fn drop(&mut self) {
        if self.paths.is_empty() {
            return;
        }

        let paths_to_delete = self.paths.clone();
        log::info!(
            "Cleanup guard is dropping. Spawning task to delete {} file(s).",
            paths_to_delete.len()
        );

        // Since `drop` is synchronous, we can't `.await` here.
        // We spawn a new async task to handle the deletion in the background.
        // This is a "fire-and-forget" task.
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
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    match send_future.await {
        Ok(_) => {
            log::info!("Successfully sent to chat_id: {}", chat_id);
        }
        Err(e) => {
            log::error!("Failed to send: Error: {:?}", e);
            // Optionally, inform the user about the failure.
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
/// This is useful for creating a canonical URL for processing and display,
/// removing tracking parameters like `?utm_source=...` or `?si=...`.
fn cleanup_url(original_url: &Url) -> Url {
    let mut cleaned_url = original_url.clone();
    cleaned_url.set_query(None);
    cleaned_url.set_fragment(None); // Also good practice to remove the #fragment
    cleaned_url
}

/// Step 1: Perform pre-download validation.
async fn pre_download_validation(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) -> Result<MediaMetadata, ()> {
    log::info!("Beginning pre-download check for {}", url);
    match downloader.get_media_metadata(url).await {
        Ok(metadata) => {
            if let Err(validation_error) = validate_media_metadata(&metadata) {
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
                Ok(metadata)
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

/// Step 2: Download the media and build the final caption.
async fn download_and_prepare_media(
    pre_download_metadata: MediaMetadata,
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) -> Result<MediaMetadata, ()> {
    match downloader.download_media(pre_download_metadata, url).await {
        Ok(mut metadata) => {
            metadata.build_caption(url);
            Ok(metadata)
        }
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
    metadata: &MediaMetadata,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    if let Some(filepath) = &metadata.filepath {
        let caption = &metadata.final_caption;
        let send_future = match metadata.telegram_media_type() {
            Some("video") => telegram_api.send_video(chat_id, message_id, filepath, caption),
            Some("photo") => telegram_api.send_photo(chat_id, message_id, filepath, caption),
            _ => {
                log::warn!(
                    "Unsupported single media type encountered for: {}",
                    filepath
                );
                let msg = "Sorry, the single media item downloaded had an unsupported type.";
                // Send the message and then return. The `_` ignores the result.
                let _ = telegram_api
                    .send_text_message(chat_id, message_id, msg)
                    .await;
                return;
            }
        };
        handle_send_operation(send_future, chat_id, message_id, telegram_api).await;
    }
}

/// Step 3 (Branch B): Handle sending a media group.
async fn send_media_group(
    metadata: &MediaMetadata,
    chat_id: ChatId,
    message_id: MessageId,
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    let media_items = metadata.entries.as_ref().unwrap(); // Should only be called if entries exist
    let mut media_group: Vec<InputMedia> = Vec::new();

    for (i, item) in media_items.iter().enumerate() {
        if let Some(filepath) = &item.filepath {
            let input_file = InputFile::file(filepath);
            let item_caption = if i == 0 {
                metadata.final_caption.clone()
            } else {
                String::new()
            };

            let media = match item.telegram_media_type() {
                Some("video") => Some(InputMedia::Video(
                    InputMediaVideo::new(input_file)
                        .parse_mode(ParseMode::Html)
                        .caption(item_caption),
                )),
                Some("photo") => Some(InputMedia::Photo(
                    InputMediaPhoto::new(input_file)
                        .parse_mode(ParseMode::Html)
                        .caption(item_caption),
                )),
                _ => {
                    log::warn!("Unsupported media type in group: {}", filepath);
                    None
                }
            };
            if let Some(m) = media {
                media_group.push(m);
            }
        }
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

// --- REFACTORED ORCHESTRATOR FUNCTION ---
pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    let clean_url = cleanup_url(url);

    let pre_download_metadata =
        match pre_download_validation(&clean_url, chat_id, message_id, downloader, telegram_api)
            .await
        {
            Ok(meta) => meta,
            Err(_) => return,
        };

    let post_download_metadata = match download_and_prepare_media(
        pre_download_metadata,
        &clean_url,
        chat_id,
        message_id,
        downloader,
        telegram_api,
    )
    .await
    {
        Ok(meta) => meta,
        Err(_) => return,
    };

    // --- File Cleanup Guard ---
    let files_to_delete: Vec<String> = if let Some(entries) = &post_download_metadata.entries {
        entries
            .iter()
            .filter_map(|item| item.filepath.clone())
            .collect()
    } else {
        post_download_metadata
            .filepath
            .clone()
            .map_or(vec![], |p| vec![p])
    };
    let _cleanup_guard = FileCleanupGuard {
        paths: files_to_delete,
    };

    // --- Dispatch to appropriate sender ---
    if post_download_metadata.entries.is_some() {
        send_media_group(&post_download_metadata, chat_id, message_id, telegram_api).await;
    } else {
        send_single_item(&post_download_metadata, chat_id, message_id, telegram_api).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MockDownloader};
    use crate::telegram_api::MockTelegramApi;
    use crate::test_utils::create_test_metadata;
    use mockall::predicate::*;
    use teloxide::types::InputMedia;
    use teloxide::types::{ChatId, MessageId};
    use url::Url;

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/valid_post").unwrap();

        let pre_download_meta = create_test_metadata();

        let meta_for_get = pre_download_meta.clone();
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| Ok(meta_for_get.clone()));

        mock_downloader
            .expect_download_media()
            .with(eq(pre_download_meta), eq(test_url.clone()))
            .times(1)
            .returning(|_metadata, _url| {
                let mut post_meta = create_test_metadata();
                post_meta.filepath = Some("/tmp/video.mp4".to_string());
                // Set the extension to signal a video
                post_meta.ext = Some("mp4".to_string());
                Ok(post_meta)
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
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
    async fn test_process_download_request_sends_photo_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/valid_photo").unwrap();
        let pre_download_meta = create_test_metadata();

        let meta_for_get = pre_download_meta.clone();
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| Ok(meta_for_get.clone()));

        mock_downloader
            .expect_download_media()
            .with(eq(pre_download_meta), eq(test_url.clone()))
            .times(1)
            .returning(|_, _| {
                let mut post_meta = create_test_metadata();
                post_meta.filepath = Some("/tmp/photo.jpg".to_string());
                // Set the extension to signal a photo
                post_meta.ext = Some("jpg".to_string());
                Ok(post_meta)
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

        let mut pre_download_meta = create_test_metadata();
        pre_download_meta.entries = Some(vec![create_test_metadata(), create_test_metadata()]);

        let meta_for_get = pre_download_meta.clone();
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| Ok(meta_for_get.clone()));

        mock_downloader
            .expect_download_media()
            .with(eq(pre_download_meta), eq(test_url.clone()))
            .times(1)
            .returning(|_, _| {
                let mut video_item = create_test_metadata();
                video_item.filepath = Some("/tmp/item1.mp4".to_string());
                video_item.ext = Some("mp4".to_string());

                let mut photo_item = create_test_metadata();
                photo_item.filepath = Some("/tmp/item2.jpg".to_string());
                photo_item.ext = Some("jpg".to_string());

                let mut result_meta = create_test_metadata();
                result_meta.entries = Some(vec![video_item, photo_item]);
                Ok(result_meta)
            });

        mock_telegram_api
            .expect_send_media_group()
            .withf(|_, _, media_vec: &Vec<InputMedia>| {
                media_vec.len() == 2
                    && matches!(&media_vec[0], InputMedia::Video(v) if v.caption.as_ref().map_or(false, |c| !c.is_empty()))
                    && matches!(&media_vec[1], InputMedia::Photo(p) if p.caption.as_ref().map_or(false, |c| c.is_empty()))
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
                let mut meta = create_test_metadata();
                meta.duration = Some(9999.0);
                Ok(meta)
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
        let pre_download_meta = create_test_metadata();

        let meta_for_get = pre_download_meta.clone();
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| Ok(meta_for_get.clone()));

        mock_downloader
            .expect_download_media()
            .with(eq(pre_download_meta), eq(test_url.clone()))
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
