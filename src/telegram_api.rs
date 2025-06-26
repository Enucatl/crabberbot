use async_trait::async_trait;
use mockall::automock;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{
        ChatId, InputFile, InputMedia, MessageId, ParseMode,
    },
};

#[automock]
#[async_trait]
pub trait TelegramApi: Send + Sync {
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
    async fn send_media_group(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        media: Vec<InputMedia>,
    ) -> Result<(), teloxide::RequestError>;
}

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
            .parse_mode(ParseMode::Html)
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
            .parse_mode(ParseMode::Html)
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

    async fn send_media_group(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        media: Vec<InputMedia>,
    ) -> Result<(), teloxide::RequestError> {
        if media.is_empty() {
            log::warn!("Attempted to send an empty media group to chat {}", chat_id);
            return Ok(()); // Or return an error if empty groups are not allowed
        }
        log::info!(
            "Sending media group ({} items) to chat {}",
            media.len(),
            chat_id
        );
        self.bot
            .send_media_group(chat_id, media)
            .reply_to(message_id)
            .await?;
        Ok(())
    }
}
