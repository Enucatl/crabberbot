use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::Downloader;
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

pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    // --- STEP 1: PRE-DOWNLOAD METADATA CHECK ---
    log::info!("Beginning pre-download check for {}", url);
    let pre_download_metadata = match downloader.get_media_metadata(url).await {
        Ok(metadata) => {
            if let Err(validation_error) = validate_media_metadata(&metadata) {
                log::warn!("Validation failed for {}: {}", url, validation_error);
                let _ = telegram_api
                    .send_text_message(chat_id, message_id, &validation_error.to_string())
                    .await;
                return;
            }
            metadata
        }
        Err(e) => {
            // If we can't even get metadata, it's probably an invalid link.
            let error_message = format!(
                "Sorry, I could not fetch information for that link. It might be private or invalid. Error: {}",
                e
            );
            let _ = telegram_api
                .send_text_message(chat_id, message_id, &error_message)
                .await;
            return;
        }
    };
    log::info!(
        "Pre-download checks passed for {}. Proceeding with download.",
        url
    );

    let mut post_download_metadata =
        match downloader.download_media(pre_download_metadata, url).await {
            Ok(metadata) => metadata,
            Err(e) => {
                let error_message = format!("Sorry, I could not download the media: {}", e);
                let _ = telegram_api
                    .send_text_message(chat_id, message_id, &error_message)
                    .await;
                return;
            }
        };

    // --- Collect file paths for cleanup ---
    // We do this first, before `result_metadata` or its fields might be moved.
    let files_to_delete: Vec<String> = if let Some(entries) = &post_download_metadata.entries {
        entries
            .iter()
            .filter_map(|item| item.filepath.clone())
            .collect()
    } else if let Some(filepath) = &post_download_metadata.filepath {
        vec![filepath.clone()]
    } else {
        // No files were downloaded, nothing to clean up.
        vec![]
    };

    post_download_metadata.build_caption(url);

    // The guard is created here. The `_` is important to bind it to the scope
    // without getting an "unused variable" warning. Cleanup is now guaranteed.
    let _cleanup_guard = FileCleanupGuard {
        paths: files_to_delete,
    };

    // Check if we have a media group (playlist) or a single item
    if let Some(media_items) = post_download_metadata.entries {
        // --- Handle Media Group ---
        let mut media_group: Vec<InputMedia> = Vec::new();
        for (i, item) in media_items.into_iter().enumerate() {
            if let Some(filepath) = &item.filepath {
                let input_file = InputFile::file(filepath);

                // The main caption is now on the top-level object
                let item_caption = if i == 0 {
                    post_download_metadata.final_caption.clone()
                } else {
                    String::new()
                };

                match item.telegram_media_type() {
                    Some("video") => {
                        media_group.push(InputMedia::Video(
                            InputMediaVideo::new(input_file)
                                .caption(item_caption)
                                .parse_mode(ParseMode::Html),
                        ));
                    }
                    Some("photo") => {
                        media_group.push(InputMedia::Photo(
                            InputMediaPhoto::new(input_file)
                                .caption(item_caption)
                                .parse_mode(ParseMode::Html),
                        ));
                    }
                    _ => {
                        log::warn!("Unsupported media type in group encountered: {}", filepath);
                        // For group sends, we typically just skip unsupported types quietly
                    }
                }
            } else {
                continue;
            }
        }

        if media_group.is_empty() {
            let _ = telegram_api
                        .send_text_message(
                            chat_id,
                            message_id,
                            "Sorry, although multiple items were found, none were of a supported type for a media group.",
                        )
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
    } else {
        // --- Handle Single Item ---
        // The result_metadata object itself represents the single downloaded file.
        if let Some(filepath) = &post_download_metadata.filepath {
            let final_caption = &post_download_metadata.final_caption;

            match post_download_metadata.telegram_media_type() {
                Some("video") => {
                    handle_send_operation(
                        telegram_api.send_video(chat_id, message_id, filepath, final_caption),
                        chat_id,
                        message_id,
                        telegram_api,
                    )
                    .await
                }
                Some("photo") => {
                    handle_send_operation(
                        telegram_api.send_photo(chat_id, message_id, filepath, final_caption),
                        chat_id,
                        message_id,
                        telegram_api,
                    )
                    .await
                }
                _ => {
                    log::warn!(
                        "Unsupported single media type encountered for: {}",
                        filepath
                    );
                    let _ = telegram_api
                        .send_text_message(
                            chat_id,
                            message_id,
                            &format!(
                                "Sorry, the single media item downloaded had an unsupported type."
                            ),
                        )
                        .await;
                }
            }
        }
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
