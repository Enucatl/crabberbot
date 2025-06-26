use async_trait::async_trait;
use mockall::automock;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{ChatId, InputFile, MessageId},
};

#[automock]
#[async_trait]
pub trait TelegramApi: Send + Sync {
    // Add Send + Sync bounds
    async fn send_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_text_message(
        &self,
        chat_id: ChatId,
        message: &str,
    ) -> Result<(), teloxide::RequestError>;
}

// The REAL implementation
#[derive(Clone)] // Clone is needed for the dispatcher
pub struct TeloxideApi {
    bot: Bot,
}

impl TeloxideApi {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }
}

#[async_trait]
impl TelegramApi for TeloxideApi {
    async fn send_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending video {} to chat {}", file_path, chat_id);
        self.bot
            .send_video(chat_id, InputFile::file(file_path))
            .caption(caption.to_string())
            .reply_to(message_id)
            .await?;
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending photo {} to chat {}", file_path, chat_id);
        self.bot
            .send_photo(chat_id, InputFile::file(file_path))
            .caption(caption.to_string())
            .reply_to(message_id)
            .await?;
        Ok(())
    }

    async fn send_text_message(
        &self,
        chat_id: ChatId,
        message: &str,
    ) -> Result<(), teloxide::RequestError> {
        self.bot.send_message(chat_id, message).await?;
        Ok(())
    }
}
