use async_trait::async_trait;
use mockall::automock;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{ChatAction, ChatId, InputFile, InputMedia, MessageId, ParseMode, ReactionType},
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
        message_id: MessageId,
        message: &str,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_media_group(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        media: Vec<InputMedia>,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_chat_action(
        &self,
        chat_id: ChatId,
        action: ChatAction,
    ) -> Result<(), teloxide::RequestError>;
    async fn set_message_reaction(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        reaction: Option<ReactionType>,
    ) -> Result<(), teloxide::RequestError>;
}

#[derive(Clone)]
pub struct TeloxideApi {
    bot: Bot,
}

impl TeloxideApi {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }

    /// Helper to determine the appropriate chat action for a media group.
    /// If any video is present, it's UploadVideo. Otherwise, it's UploadPhoto.
    fn get_media_group_action(media: &[InputMedia]) -> ChatAction {
        if media
            .iter()
            .any(|item| matches!(item, InputMedia::Video(_)))
        {
            ChatAction::UploadVideo
        } else {
            ChatAction::UploadPhoto
        }
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
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
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
        self.send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await?;
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
        message_id: MessageId,
        message: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending text to chat {}", chat_id);
        self.bot
            .send_message(chat_id, message)
            .parse_mode(ParseMode::Html)
            .reply_to(message_id)
            .await?;
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
            return Ok(());
        }
        log::info!(
            "Sending media group ({} items) to chat {}",
            media.len(),
            chat_id
        );
        let action = Self::get_media_group_action(&media);
        self.send_chat_action(chat_id, action).await?;
        self.bot
            .send_media_group(chat_id, media)
            .reply_to(message_id)
            .await?;
        Ok(())
    }

    async fn send_chat_action(
        &self,
        chat_id: ChatId,
        action: ChatAction,
    ) -> Result<(), teloxide::RequestError> {
        self.bot.send_chat_action(chat_id, action).await?;
        Ok(())
    }

    async fn set_message_reaction(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        reaction: Option<ReactionType>,
    ) -> Result<(), teloxide::RequestError> {
        self.bot
            .set_message_reaction(chat_id, message_id)
            .reaction(reaction)
            .is_big(true)
            .await?;
        Ok(())
    }
}
