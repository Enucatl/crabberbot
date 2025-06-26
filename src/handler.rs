use teloxide::types::{ChatId, MessageId};
use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, ParseMode};

use crate::downloader::{Downloader, MediaMetadata};
use crate::telegram_api::TelegramApi;

pub async fn process_download_request(
    url: &str,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    match downloader.download_media(url).await {
        Ok((caption, media_items)) => {
            if media_items.is_empty() {
                let _ = telegram_api
                    .send_text_message(chat_id, "Sorry, no media files were found for this link.")
                    .await;
                return;
            }

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

            if media_items.len() == 1 {
                let item = media_items.into_iter().next().unwrap();
                let file_path = &item.filepath;
                let final_caption = &caption;

                match get_telegram_media_type(&item) {
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
                            item.filepath
                        );
                        let _ = telegram_api.send_text_message(
                            chat_id,
                            &format!("Sorry, the single media item downloaded had an unsupported type ({}).", item.ext),
                        ).await;
                    }
                }
            } else {
                let mut media_group: Vec<InputMedia> = Vec::new();
                for (i, item) in media_items.into_iter().enumerate() {
                    let input_file = InputFile::file(item.filepath.clone());
                    let item_caption = if i == 0 {
                        caption.clone()
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
                            "Sorry, although multiple items were found, none were of a supported type for a media group.",
                        )
                        .await;
                } else {
                    let _ = telegram_api
                        .send_media_group(chat_id, message_id, media_group)
                        .await;
                }
            }
        }
        Err(e) => {
            let error_message = format!("Sorry, I could not process the link: {}", e);
            let _ = telegram_api
                .send_text_message(chat_id, &error_message)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::{DownloadError, MediaMetadata, MockDownloader};
    use crate::telegram_api::MockTelegramApi;
    use mockall::predicate::*;
    use teloxide::types::{ChatId, MessageId};
    use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, ParseMode};

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success_with_type() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/valid_post";
        let video_title = "My Awesome Video";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(move |_| {
                Ok((
                    video_title.to_string(),
                    vec![MediaMetadata {
                        filepath: "/tmp/video.mp4".to_string(),
                        description: "A detailed description of the video.".to_string(),
                        title: video_title.to_string(),
                        ext: "mp4".to_string(),
                        media_type: Some("video".to_string()), // Explicitly set _type
                        resolution: None,
                        width: None,
                        height: None,
                    }],
                ))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
                eq(video_title),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        process_download_request(
            test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_photo_on_success_with_type() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/valid_photo";
        let photo_title = "Beautiful Sunset";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(move |_| {
                Ok((
                    photo_title.to_string(),
                    vec![MediaMetadata {
                        filepath: "/tmp/photo.jpg".to_string(),
                        description: "Detailed description of the sunset.".to_string(),
                        title: photo_title.to_string(),
                        ext: "jpg".to_string(),
                        media_type: Some("image".to_string()), // Explicitly set _type
                        resolution: None,
                        width: None,
                        height: None,
                    }],
                ))
            });

        mock_telegram_api
            .expect_send_photo()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/photo.jpg"),
                eq(photo_title),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        process_download_request(
            test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_sends_error_on_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/invalid_post";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(|_| Err(DownloadError::CommandFailed("Failed".to_string())));

        mock_telegram_api
            .expect_send_text_message()
            .withf(|chat_id, msg| *chat_id == ChatId(123) && msg.contains("could not process"))
            .times(1)
            .returning(|_, _| Ok(()));

        mock_telegram_api.expect_send_video().times(0);
        mock_telegram_api.expect_send_photo().times(0);
        mock_telegram_api.expect_send_media_group().times(0);

        process_download_request(
            test_url,
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
        let test_url = "https://instagram.com/p/multiple_media";
        let main_post_title = "My Album Title";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(move |_| {
                Ok((
                    main_post_title.to_string(),
                    vec![
                        MediaMetadata {
                            filepath: "/tmp/item1.mp4".to_string(),
                            description: "First video description".to_string(),
                            title: main_post_title.to_string(),
                            ext: "mp4".to_string(),
                            media_type: Some("video".to_string()),
                            resolution: None,
                            width: None,
                            height: None,
                        },
                        MediaMetadata {
                            filepath: "/tmp/item2.jpg".to_string(),
                            description: "Second image description".to_string(),
                            title: main_post_title.to_string(),
                            ext: "jpg".to_string(),
                            media_type: Some("image".to_string()),
                            resolution: None,
                            width: None,
                            height: None,
                        },
                    ],
                ))
            });

        mock_telegram_api
            .expect_send_media_group()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                function(move |media_vec: &Vec<InputMedia>| {
                    media_vec.len() == 2
                        && if let Some(InputMedia::Video(v)) = media_vec.get(0) {
                            v.caption.as_deref() == Some(main_post_title)
                                && v.parse_mode == Some(ParseMode::Html)
                        } else {
                            false // Not a file input
                        }
                        && if let Some(InputMedia::Photo(p)) = media_vec.get(1) {
                            p.caption.as_deref() == Some("")
                                && p.parse_mode == Some(ParseMode::Html)
                        } else {
                            false // Not a file input
                        }
                }),
            )
            .times(1)
            .returning(|_, _, _| Ok(()));

        process_download_request(
            test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }

    #[tokio::test]
    async fn test_process_download_request_no_supported_media() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://example.com/unsupported";
        let title_of_unsupported = "Unsupported File";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(move |_| {
                Ok((
                    title_of_unsupported.to_string(),
                    vec![MediaMetadata {
                        filepath: "/tmp/document.pdf".to_string(),
                        description: "A PDF document".to_string(),
                        title: title_of_unsupported.to_string(),
                        ext: "pdf".to_string(),
                        media_type: Some("document".to_string()), // Explicitly set _type
                        resolution: None,
                        width: None,
                        height: None,
                    }],
                ))
            });

        mock_telegram_api
            .expect_send_text_message()
            .withf(|chat_id, msg| *chat_id == ChatId(123) && msg.contains("unsupported type"))
            .times(1)
            .returning(|_, _| Ok(()));

        mock_telegram_api.expect_send_video().times(0);
        mock_telegram_api.expect_send_photo().times(0);
        mock_telegram_api.expect_send_media_group().times(0);

        process_download_request(
            test_url,
            ChatId(123),
            MessageId(456),
            &mock_downloader,
            &mock_telegram_api,
        )
        .await;
    }
}
