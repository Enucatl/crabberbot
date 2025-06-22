use crate::downloader::Downloader;
use crate::telegram_api::TelegramApi;
use teloxide::types::{ChatId, MessageId};

pub async fn message_handler(
    text: &str,
    chat_id: ChatId,
    message_id: MessageId,
    downloader: &dyn Downloader,
    telegram_api: &dyn TelegramApi,
) {
    if !text.starts_with("http") {
        return;
    }

    match downloader.download_media(text).await {
        Ok((caption, file_paths)) => {
            for path in file_paths {
                if path.ends_with(".mp4") {
                    let _ = telegram_api.send_video(chat_id, message_id, &path, &caption).await;
                } else if path.ends_with(".jpg") || path.ends_with(".png") {
                    let _ = telegram_api.send_photo(chat_id, message_id, &path, &caption).await;
                }
            }
        }
        Err(e) => {
            let error_message = format!("Sorry, I could not process that link: {}", e);
            let _ = telegram_api.send_error_message(chat_id, &error_message).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*; // Import things from the parent module (handler)
    use crate::downloader::{MockDownloader, DownloadError}; 
    use crate::telegram_api::MockTelegramApi;
    use mockall::predicate::*;

    #[tokio::test]
    async fn test_handler_sends_video_on_success() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/valid_post";

        mock_downloader.expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(|_| Ok(("A great video!".to_string(), vec!["/tmp/video.mp4".to_string()])));

        mock_telegram_api.expect_send_video()
            .with(eq(ChatId(123)), eq(MessageId(456)), eq("/tmp/video.mp4"), eq("A great video!"))
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        message_handler(test_url, ChatId(123), MessageId(456), &mock_downloader, &mock_telegram_api).await;
    }

    #[tokio::test]
    async fn test_handler_sends_error_on_failure() {
        let mut mock_downloader = MockDownloader::new();
        let mut mock_telegram_api = MockTelegramApi::new();
        let test_url = "https://instagram.com/p/invalid_post";

        mock_downloader.expect_download_media()
            .with(eq(test_url))
            .times(1)
            .returning(|_| Err(DownloadError::CommandFailed("couldn't download media".to_string())));

        mock_telegram_api.expect_send_error_message()
            .withf(|chat_id, msg| *chat_id == ChatId(123) && msg.contains("not find media"))
            .times(1)
            .returning(|_, _| Ok(()));
        
        mock_telegram_api.expect_send_video().times(0);

        message_handler(test_url, ChatId(123), MessageId(456), &mock_downloader, &mock_telegram_api).await;
    }
}
