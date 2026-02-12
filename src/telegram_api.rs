use std::path::{Path, PathBuf};

use async_trait::async_trait;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{ChatAction, ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, MessageId, ParseMode, ReactionType},
};

use crate::downloader::MediaType;
use crate::storage::CachedFile;

#[derive(Debug, Clone)]
pub struct SentMedia {
    pub file_id: String,
    pub media_type: MediaType,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TelegramApi: Send + Sync {
    async fn send_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &Path,
        caption: &str,
        thumbnail_filepath: Option<PathBuf>,
    ) -> Result<String, teloxide::RequestError>;
    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &Path,
        caption: &str,
    ) -> Result<String, teloxide::RequestError>;
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
    ) -> Result<Vec<SentMedia>, teloxide::RequestError>;
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
    async fn send_cached_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_id: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_cached_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_id: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError>;
    async fn send_cached_media_group(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        files: &[CachedFile],
        caption: &str,
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
        file_path: &Path,
        caption: &str,
        thumbnail_filepath: Option<PathBuf>,
    ) -> Result<String, teloxide::RequestError> {
        log::info!("Sending video {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
        let mut request = self
            .bot
            .send_video(chat_id, InputFile::file(file_path))
            .caption(caption.to_string())
            .parse_mode(ParseMode::Html)
            .reply_to(message_id);

        if let Some(p) = thumbnail_filepath {
            request = request.thumbnail(InputFile::file(p));
        }
        let message = request.await?;
        let file_id = message
            .video()
            .map(|v| v.file.id.to_string())
            .unwrap_or_default();
        Ok(file_id)
    }

    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &Path,
        caption: &str,
    ) -> Result<String, teloxide::RequestError> {
        log::info!("Sending photo {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await?;
        let message = self
            .bot
            .send_photo(chat_id, InputFile::file(file_path))
            .caption(caption.to_string())
            .parse_mode(ParseMode::Html)
            .reply_to(message_id)
            .await?;
        let file_id = message
            .photo()
            .and_then(|photos| photos.last())
            .map(|p| p.file.id.to_string())
            .unwrap_or_default();
        Ok(file_id)
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
    ) -> Result<Vec<SentMedia>, teloxide::RequestError> {
        if media.is_empty() {
            log::warn!("Attempted to send an empty media group to chat {}", chat_id);
            return Ok(vec![]);
        }
        log::info!(
            "Sending media group ({} items) to chat {}",
            media.len(),
            chat_id
        );
        let action = Self::get_media_group_action(&media);
        self.send_chat_action(chat_id, action).await?;
        let messages = self
            .bot
            .send_media_group(chat_id, media)
            .reply_to(message_id)
            .await?;

        let sent: Vec<SentMedia> = messages
            .iter()
            .filter_map(|msg| {
                if let Some(video) = msg.video() {
                    Some(SentMedia {
                        file_id: video.file.id.to_string(),
                        media_type: MediaType::Video,
                    })
                } else if let Some(photos) = msg.photo() {
                    photos.last().map(|p| SentMedia {
                        file_id: p.file.id.to_string(),
                        media_type: MediaType::Photo,
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(sent)
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

    async fn send_cached_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_id: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending cached video to chat {}", chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
        self.bot
            .send_video(chat_id, InputFile::file_id(String::from(file_id).into()))
            .caption(caption.to_string())
            .parse_mode(ParseMode::Html)
            .reply_to(message_id)
            .await?;
        Ok(())
    }

    async fn send_cached_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_id: &str,
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending cached photo to chat {}", chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await?;
        self.bot
            .send_photo(chat_id, InputFile::file_id(String::from(file_id).into()))
            .caption(caption.to_string())
            .parse_mode(ParseMode::Html)
            .reply_to(message_id)
            .await?;
        Ok(())
    }

    async fn send_cached_media_group(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        files: &[CachedFile],
        caption: &str,
    ) -> Result<(), teloxide::RequestError> {
        if files.is_empty() {
            return Ok(());
        }
        log::info!(
            "Sending cached media group ({} items) to chat {}",
            files.len(),
            chat_id
        );

        let media: Vec<InputMedia> = files
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let input_file = InputFile::file_id(file.telegram_file_id.clone().into());
                let item_caption = if i == 0 {
                    caption.to_string()
                } else {
                    String::new()
                };
                match file.media_type {
                    MediaType::Video => InputMedia::Video(
                        InputMediaVideo::new(input_file)
                            .parse_mode(ParseMode::Html)
                            .caption(item_caption),
                    ),
                    MediaType::Photo => InputMedia::Photo(
                        InputMediaPhoto::new(input_file)
                            .parse_mode(ParseMode::Html)
                            .caption(item_caption),
                    ),
                }
            })
            .collect();

        let action = Self::get_media_group_action(&media);
        self.send_chat_action(chat_id, action).await?;
        self.bot
            .send_media_group(chat_id, media)
            .reply_to(message_id)
            .await?;
        Ok(())
    }
}
