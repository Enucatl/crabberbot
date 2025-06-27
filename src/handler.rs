use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode,
};
use url::Url;

use crate::downloader::{Downloader, MediaMetadata};
use crate::telegram_api::TelegramApi;

pub async fn process_download_request(
    url: &Url,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    match downloader.download_media(url).await {
        Ok((caption, media_items)) => {
            if media_items.is_empty() {
                let _ = telegram_api
                    .send_text_message(
                        chat_id,
                        message_id,
                        "Sorry, no media files were found for this link.",
                    )
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
                            message_id,
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
                            message_id,
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
    use mockall::predicate::*;
    use teloxide::types::{ChatId, MessageId};
    use teloxide::types::{InputFile, InputMedia, ParseMode};
    use url::Url;

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success_with_type() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/valid_post").expect("");
        let video_uploader = "testuser";
        let video_description = "A detailed description of the video.";
        let expected_caption = "caption";
        let downloader_caption = expected_caption;

        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok((
                    downloader_caption.to_string(),
                    vec![MediaMetadata {
                        filepath: "/tmp/video.mp4".to_string(),
                        description: video_description.to_string(),
                        title: "My Awesome Video".to_string(),
                        ext: "mp4".to_string(),
                        uploader: Some(video_uploader.to_string()),
                        media_type: Some("video".to_string()),
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
                eq(expected_caption),
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
    async fn test_process_download_request_sends_photo_on_success_with_type() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/valid_photo").expect("");
        let photo_uploader = "photographer123";
        let photo_description = "Detailed description of the sunset.";
        let expected_caption = format!(
            "<a href=\"{}\">Source</a> 🦀 <a href=\"https://t.me/crabberbot?start=c\">Via</a>\n\n<blockquote>@{}\n{}</blockquote>",
            test_url, photo_uploader, photo_description
        );
        let downloader_caption = expected_caption.clone();

        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok((
                    downloader_caption.clone(),
                    vec![MediaMetadata {
                        filepath: "/tmp/photo.jpg".to_string(),
                        description: photo_description.to_string(),
                        title: "Beautiful Sunset".to_string(),
                        ext: "jpg".to_string(),
                        uploader: Some(photo_uploader.to_string()),
                        media_type: Some("image".to_string()),
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
                eq(expected_caption),
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
    async fn test_process_download_request_sends_error_on_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/invalid_post").expect("");

        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(|_| Err(DownloadError::CommandFailed("Failed".to_string())));

        mock_telegram_api
            .expect_send_text_message()
            .withf(|chat_id, message_id, msg| {
                *chat_id == ChatId(123)
                    && *message_id == MessageId(456)
                    && msg.contains("could not process")
            })
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

    #[tokio::test]
    async fn test_process_download_request_sends_media_group_on_multiple_items() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://instagram.com/p/multiple_media").expect("");
        let expected_caption = "caption";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok((
                    expected_caption.to_string(),
                    vec![
                        MediaMetadata {
                            filepath: "/tmp/item1.mp4".to_string(),
                            description: "First video description".to_string(),
                            title: "My Album Title".to_string(),
                            uploader: Some("album_creator".to_string()),
                            ext: "mp4".to_string(),
                            media_type: Some("video".to_string()),
                            resolution: None,
                            width: None,
                            height: None,
                        },
                        MediaMetadata {
                            filepath: "/tmp/item2.jpg".to_string(),
                            description: "Second image description".to_string(),
                            title: "My Album Title".to_string(),
                            ext: "jpg".to_string(),
                            uploader: Some("album_creator".to_string()),
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
                            format!("{:?}", v.media)
                                == format!("{:?}", InputFile::file("/tmp/item1.mp4"))
                                && v.parse_mode == Some(ParseMode::Html)
                        } else {
                            false
                        }
                        && if let Some(InputMedia::Photo(p)) = media_vec.get(1) {
                            format!("{:?}", p.media)
                                == format!("{:?}", InputFile::file("/tmp/item2.jpg"))
                                && p.parse_mode == Some(ParseMode::Html)
                        } else {
                            false
                        }
                }),
            )
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
    async fn test_process_download_request_no_supported_media() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = Url::parse("https://example.com/unsupported").expect("");
        let title_of_unsupported = "Unsupported File";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url.clone()))
            .times(1)
            .returning(move |_| {
                Ok((
                    title_of_unsupported.to_string(),
                    vec![MediaMetadata {
                        filepath: "/tmp/document.pdf".to_string(),
                        description: "A PDF document".to_string(),
                        title: title_of_unsupported.to_string(),
                        uploader: None,
                        ext: "pdf".to_string(),
                        media_type: Some("document".to_string()),
                        resolution: None,
                        width: None,
                        height: None,
                    }],
                ))
            });

        mock_telegram_api
            .expect_send_text_message()
            .withf(|chat_id, message_id, msg| {
                *chat_id == ChatId(123)
                    && *message_id == MessageId(456)
                    && msg.contains("unsupported type")
            })
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
