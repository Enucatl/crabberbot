use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::{Downloader, MediaMetadata};
use crate::telegram_api::TelegramApi;
use crate::validator::validate_media_metadata;

pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    // --- STEP 1: PRE-DOWNLOAD METADATA CHECK ---
    log::info!("Beginning pre-download check for {}", url);
    match downloader.get_media_metadata(url).await {
        Ok(metadata) => {
            // Here is the refactored part. We call our single validation function.
            if let Err(validation_error) = validate_media_metadata(&metadata) {
                // The error contains a user-friendly message, so we just send it.
                log::warn!("Validation failed for {}: {}", url, validation_error);
                let _ = telegram_api
                    .send_text_message(chat_id, message_id, &validation_error.to_string())
                    .await;
                return;
            }
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
    }
    log::info!(
        "Pre-download checks passed for {}. Proceeding with download.",
        url
    );

    match downloader.download_media(url).await {
        Ok(result_metadata) => {
            // Helper closure to determine media type
            let get_telegram_media_type = |item: &MediaMetadata| {
                if let Some(media_type_str) = &item.media_type {
                    match media_type_str.as_str() {
                        "video" => Some("video"),
                        "image" => Some("photo"),
                        _ => None, // Unknown _type, fall through to ext check
                    }
                } else {
                    // _type not available, fallback to ext check
                    match item.ext.as_str() {
                        "mp4" | "webm" | "gif" | "mov" => Some("video"),
                        "jpg" | "jpeg" | "png" | "webp" => Some("photo"),
                        _ => None, // Unsupported extension
                    }
                }
            };

            // Check if we have a media group (playlist) or a single item
            if let Some(media_items) = result_metadata.entries {
                // --- Handle Media Group ---
                let mut media_group: Vec<InputMedia> = Vec::new();
                for (i, item) in media_items.into_iter().enumerate() {
                    let input_file = InputFile::file(item.filepath.clone());
                    // The main caption is now on the top-level object
                    let item_caption = if i == 0 {
                        result_metadata.final_caption.clone()
                    } else {
                        String::new()
                    };

                    match get_telegram_media_type(&item) {
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
                            log::warn!(
                                "Unsupported media type in group encountered: {}",
                                item.filepath
                            );
                            // For group sends, we typically just skip unsupported types quietly
                        }
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
                    let _ = telegram_api
                        .send_media_group(chat_id, message_id, media_group)
                        .await;
                }
            } else {
                // --- Handle Single Item ---
                // The result_metadata object itself represents the single downloaded file.
                let file_path = &result_metadata.filepath;
                let final_caption = &result_metadata.final_caption;

                match get_telegram_media_type(&result_metadata) {
                    Some("video") => {
                        let _ = telegram_api
                            .send_video(chat_id, message_id, file_path, final_caption)
                            .await;
                    }
                    Some("photo") => {
                        let _ = telegram_api
                            .send_photo(chat_id, message_id, file_path, final_caption)
                            .await;
                    }
                    _ => {
                        log::warn!(
                            "Unsupported single media type encountered for: {}",
                            result_metadata.filepath
                        );
                        let _ = telegram_api.send_text_message(
                            chat_id,
                            message_id,
                            &format!("Sorry, the single media item downloaded had an unsupported type ({}).", result_metadata.ext),
                        ).await;
                    }
                }
            }
        }
        Err(e) => {
            let error_message = format!("Sorry, I could not process the link: {}", e);
            let _ = telegram_api
                .send_text_message(chat_id, message_id, &error_message)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MediaMetadata, MockDownloader};
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
        let expected_caption = "caption_for_video".to_string();

        // --- Setup Mocks ---

        // 1. Mock the PRE-DOWNLOAD check. We'll say it passes.
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_metadata())); // Return valid, empty metadata

        // 2. Mock the ACTUAL DOWNLOAD. Now returns a single MediaMetadata object.
        let returned_caption = expected_caption.clone();
        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok(MediaMetadata {
                    filepath: "/tmp/video.mp4".to_string(),
                    ext: "mp4".to_string(),
                    media_type: Some("video".to_string()),
                    final_caption: returned_caption.clone(),
                    ..create_test_metadata() // Use the helper for all other fields
                })
            });

        // 3. Mock the Telegram API call we expect to happen.
        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
                eq(expected_caption),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        // --- Run Test ---
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
        let expected_caption = "caption_for_photo".to_string();

        // Mock pre-download check
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_metadata()));

        // Mock actual download
        let returned_caption = expected_caption.clone();
        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok(MediaMetadata {
                    filepath: "/tmp/photo.jpg".to_string(),
                    ext: "jpg".to_string(),
                    media_type: Some("image".to_string()),
                    final_caption: returned_caption.clone(),
                    ..create_test_metadata()
                })
            });

        // Mock Telegram API
        mock_telegram_api
            .expect_send_photo()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/photo.jpg"),
                eq(expected_caption),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        // Run Test
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
        let expected_caption = "main caption".to_string();

        // Mock pre-download check
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| {
                Ok(MediaMetadata {
                    // Say it's a playlist of 2 items.
                    entries: Some(vec![create_test_metadata(), create_test_metadata()]),
                    ..create_test_metadata()
                })
            });

        // Mock actual download
        let returned_caption = expected_caption.clone();
        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                // Return a single metadata object with the `entries` field populated.
                Ok(MediaMetadata {
                    final_caption: returned_caption.clone(),
                    entries: Some(vec![
                        MediaMetadata {
                            filepath: "/tmp/item1.mp4".to_string(),
                            media_type: Some("video".to_string()),
                            ..create_test_metadata()
                        },
                        MediaMetadata {
                            filepath: "/tmp/item2.jpg".to_string(),
                            media_type: Some("image".to_string()),
                            ..create_test_metadata()
                        },
                    ]),
                    ..create_test_metadata()
                })
            });

        // Mock Telegram API
        mock_telegram_api
            .expect_send_media_group()
            .withf(move |_, _, media_vec: &Vec<InputMedia>| {
                // Check the media group contents
                media_vec.len() == 2 &&
                // Check first item
                matches!(&media_vec[0], InputMedia::Video(v) if v.caption == Some(expected_caption.clone())) &&
                // Check second item
                matches!(&media_vec[1], InputMedia::Photo(p) if p.caption == Some("".to_string()))
            })
            .times(1)
            .returning(|_, _, _| Ok(()));

        // Run Test
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

        // Mock the pre-download check to return a video that is too long
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| {
                Ok(MediaMetadata {
                    duration: Some(9999.0), // A very long duration
                    ..create_test_metadata()
                })
            });

        // The actual download should NEVER be called.
        mock_downloader.expect_download_media().times(0);

        // We expect a text message explaining the failure.
        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| msg.contains("too long"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        // Run Test
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

        // Mock the pre-download check to succeed
        mock_downloader
            .expect_get_media_metadata()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Ok(create_test_metadata()));

        // Mock the actual download to fail
        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Err(DownloadError::CommandFailed("yt-dlp exploded".to_string())));

        // We expect an error message
        mock_telegram_api
            .expect_send_text_message()
            .withf(|_, _, msg| msg.contains("could not process the link"))
            .times(1)
            .returning(|_, _, _| Ok(()));

        // No media should be sent
        mock_telegram_api.expect_send_video().times(0);
        mock_telegram_api.expect_send_photo().times(0);
        mock_telegram_api.expect_send_media_group().times(0);

        // Run Test
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
