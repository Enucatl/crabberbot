use teloxide::types::{ChatId, MessageId};

use crate::downloader::Downloader;
use crate::telegram_api::TelegramApi;

pub async fn process_download_request(
    url: &str,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &(dyn Downloader + Send + Sync),
    telegram_api: &(dyn TelegramApi + Send + Sync),
) {
    match downloader.download_media(url).await {
        Ok((caption, file_paths)) => {
            for path in file_paths {
                if path.ends_with(".mp4") {
                    let _ = telegram_api
                        .send_video(chat_id, message_id, &path, &caption)
                        .await;
                } else if path.ends_with(".jpg") || path.ends_with(".png") {
                    let _ = telegram_api
                        .send_photo(chat_id, message_id, &path, &caption)
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
    use super::*; // Import things from the parent module (handler)
    use crate::downloader::{DownloadError, MockDownloader};
    use crate::telegram_api::MockTelegramApi;
    use mockall::predicate::*;
    use teloxide::types::{ChatId, MessageId}; // Added for tests

    #[tokio::test]
    async fn test_process_download_request_sends_video_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/valid_post";

        mock_downloader
            .expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(|_| {
                Ok((
                    "A great video!".to_string(),
                    vec!["/tmp/video.mp4".to_string()],
                ))
            });

        mock_telegram_api
            .expect_send_video()
            .with(
                eq(ChatId(123)),
                eq(MessageId(456)),
                eq("/tmp/video.mp4"),
                eq("A great video!"),
            )
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        // Call the renamed function
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

        mock_telegram_api.expect_send_video().times(0); // Ensure no video is sent

        // Call the renamed function
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
