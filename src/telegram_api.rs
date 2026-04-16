use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use image::GenericImageView;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{
        ChatAction, ChatId, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaPhoto,
        InputMediaVideo, MessageId, ParseMode, ReactionType, TelegramTransactionId, UserId,
    },
};
use tokio::sync::Mutex;

use crate::downloader::MediaType;
use crate::retry::{RetryPolicy, retry_async};
use crate::storage::CachedFile;

const TELEGRAM_MAX_DIMENSION_SUM: u32 = 10_000;
const MAX_PHOTO_WIDTH: u32 = 12_000;
const MAX_PHOTO_HEIGHT: u32 = 12_000;
const MAX_PHOTO_PIXELS: u64 = 48_000_000;
const MAX_PHOTO_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// Resize a photo if its dimension sum exceeds Telegram's 10000 limit.
/// Returns the path to a temporary resized file, or None if no resize was needed.
/// The caller is responsible for deleting the temp file when done.
pub(crate) fn resize_photo_if_needed(path: &Path) -> Result<Option<PathBuf>, String> {
    let dimensions = match image::ImageReader::open(path)
        .map_err(|e| e.to_string())
        .and_then(|reader| reader.with_guessed_format().map_err(|e| e.to_string()))
        .and_then(|mut reader| {
            reader.limits(image_limits());
            reader.into_dimensions().map_err(|e| e.to_string())
        }) {
        Ok(dimensions) => dimensions,
        Err(e) => {
            log::warn!("Could not read image dimensions for {:?}: {}", path, e);
            return Ok(None);
        }
    };
    let (w, h) = dimensions;
    if !photo_dimensions_allowed(w, h) {
        log::warn!(
            "Rejecting photo {:?}: dimensions {}x{} exceed policy",
            path,
            w,
            h
        );
        return Err(format!(
            "Photo dimensions {}x{} exceed the configured safety limit.",
            w, h
        ));
    }

    if w + h <= TELEGRAM_MAX_DIMENSION_SUM {
        return Ok(None);
    }

    let img = match image::ImageReader::open(path)
        .map_err(|e| e.to_string())
        .and_then(|reader| reader.with_guessed_format().map_err(|e| e.to_string()))
        .and_then(|mut reader| {
            reader.limits(image_limits());
            reader.decode().map_err(|e| e.to_string())
        }) {
        Ok(img) => img,
        Err(e) => {
            log::warn!("Could not decode {:?} for resize: {}", path, e);
            return Ok(None);
        }
    };
    let (w, h) = img.dimensions();
    let scale = f64::from(TELEGRAM_MAX_DIMENSION_SUM - 1) / f64::from(w + h);
    let new_w = (w as f64 * scale).round() as u32;
    let new_h = (h as f64 * scale).round() as u32;
    log::info!(
        "Resizing photo from {}x{} to {}x{} before sending to Telegram",
        w,
        h,
        new_w,
        new_h
    );
    let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
    let temp_path = std::env::temp_dir().join(format!("{}.{}", uuid::Uuid::new_v4(), ext));
    if let Err(e) = resized.save(&temp_path) {
        log::warn!("Could not save resized image to {:?}: {}", temp_path, e);
        return Ok(None);
    }
    Ok(Some(temp_path))
}

pub(crate) fn photo_dimensions_allowed(width: u32, height: u32) -> bool {
    width <= MAX_PHOTO_WIDTH
        && height <= MAX_PHOTO_HEIGHT
        && u64::from(width) * u64::from(height) <= MAX_PHOTO_PIXELS
}

fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PHOTO_WIDTH);
    limits.max_image_height = Some(MAX_PHOTO_HEIGHT);
    limits.max_alloc = Some(MAX_PHOTO_DECODE_BYTES);
    limits
}

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
    ) -> Result<(String, MessageId), teloxide::RequestError>;
    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &Path,
        caption: &str,
    ) -> Result<(String, MessageId), teloxide::RequestError>;
    async fn edit_message_reply_markup(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        keyboard: InlineKeyboardMarkup,
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
    ) -> Result<MessageId, teloxide::RequestError>;
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

    async fn send_audio(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &std::path::Path,
    ) -> Result<(), teloxide::RequestError>;

    async fn send_invoice(
        &self,
        chat_id: ChatId,
        title: &str,
        description: &str,
        payload: &str,
        price_amount: u32,
    ) -> Result<(), teloxide::RequestError>;

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<String>,
    ) -> Result<(), teloxide::RequestError>;

    async fn answer_pre_checkout_query(
        &self,
        pre_checkout_query_id: &str,
        ok: bool,
        error_message: Option<String>,
    ) -> Result<(), teloxide::RequestError>;

    async fn send_text_with_keyboard(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: &str,
        keyboard: InlineKeyboardMarkup,
    ) -> Result<(), teloxide::RequestError>;

    /// Send a text message without replying to any specific message.
    /// Used for outbound relay messages to the owner's chat.
    async fn send_text_no_reply(
        &self,
        chat_id: ChatId,
        text: &str,
    ) -> Result<(), teloxide::RequestError>;

    /// Refund a Telegram Stars payment. user_id is the payer's Telegram user ID.
    async fn refund_star_payment(
        &self,
        user_id: i64,
        telegram_payment_charge_id: &str,
    ) -> Result<(), teloxide::RequestError>;
}

#[derive(Clone)]
pub struct TeloxideApi {
    bot: Bot,
    limiter: Arc<TelegramRequestLimiter>,
    retry_policy: RetryPolicy,
}

impl TeloxideApi {
    pub fn new(bot: Bot) -> Self {
        Self {
            bot,
            limiter: Arc::new(TelegramRequestLimiter::new()),
            retry_policy: RetryPolicy {
                max_attempts: 4,
                base_delay: Duration::from_millis(250),
                max_delay: Duration::from_secs(30),
            },
        }
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

    async fn request<T, Fut, Op>(
        &self,
        chat_id: Option<ChatId>,
        label: &'static str,
        mut op: Op,
    ) -> Result<T, teloxide::RequestError>
    where
        Op: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, teloxide::RequestError>>,
    {
        let limiter = self.limiter.clone();
        retry_async(
            &self.retry_policy,
            || {
                let limiter = limiter.clone();
                let future = op();
                async move {
                    limiter.wait(chat_id).await;
                    future.await
                }
            },
            |error| match error {
                teloxide::RequestError::RetryAfter(after) => Some(after.duration()),
                _ => None,
            },
            |error| {
                matches!(
                    error,
                    teloxide::RequestError::RetryAfter(_)
                        | teloxide::RequestError::Network(_)
                        | teloxide::RequestError::InvalidJson { .. }
                )
            },
            label,
        )
        .await
    }
}

struct TelegramRequestLimiter {
    global_next: Mutex<Instant>,
    chat_next: DashMap<i64, Arc<Mutex<Instant>>>,
}

impl TelegramRequestLimiter {
    fn new() -> Self {
        Self {
            global_next: Mutex::new(Instant::now()),
            chat_next: DashMap::new(),
        }
    }

    async fn wait(&self, chat_id: Option<ChatId>) {
        wait_slot(&self.global_next, Duration::from_millis(34)).await;
        if let Some(chat_id) = chat_id {
            let chat_mutex = self
                .chat_next
                .entry(chat_id.0)
                .or_insert_with(|| Arc::new(Mutex::new(Instant::now())))
                .clone();
            wait_slot(&chat_mutex, Duration::from_millis(1_100)).await;
        }
    }
}

async fn wait_slot(next: &Mutex<Instant>, spacing: Duration) {
    let mut guard = next.lock().await;
    let now = Instant::now();
    if *guard > now {
        tokio::time::sleep(*guard - now).await;
    }
    *guard = Instant::now() + spacing;
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
    ) -> Result<(String, MessageId), teloxide::RequestError> {
        log::info!("Sending video {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
        let message = self
            .request(Some(chat_id), "telegram.send_video", || {
                let mut request = self
                    .bot
                    .send_video(chat_id, InputFile::file(file_path))
                    .caption(caption.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id);

                if let Some(p) = thumbnail_filepath.clone() {
                    request = request.thumbnail(InputFile::file(p));
                }
                async move { request.await }
            })
            .await?;
        let file_id = message
            .video()
            .map(|v| v.file.id.to_string())
            .ok_or_else(|| {
                log::warn!("send_video: Telegram response missing video file_id");
                teloxide::RequestError::Api(teloxide::ApiError::Unknown(
                    "Missing file_id in Telegram response".to_owned(),
                ))
            })?;
        Ok((file_id, message.id))
    }

    async fn send_photo(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &Path,
        caption: &str,
    ) -> Result<(String, MessageId), teloxide::RequestError> {
        log::info!("Sending photo {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await?;
        let message = self
            .request(Some(chat_id), "telegram.send_photo", || async {
                self.bot
                    .send_photo(chat_id, InputFile::file(file_path))
                    .caption(caption.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id)
                    .await
            })
            .await?;
        let file_id = message
            .photo()
            .and_then(|photos| photos.last())
            .map(|p| p.file.id.to_string())
            .ok_or_else(|| {
                log::warn!("send_photo: Telegram response missing photo file_id");
                teloxide::RequestError::Api(teloxide::ApiError::Unknown(
                    "Missing file_id in Telegram response".to_owned(),
                ))
            })?;
        Ok((file_id, message.id))
    }

    async fn edit_message_reply_markup(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        keyboard: InlineKeyboardMarkup,
    ) -> Result<(), teloxide::RequestError> {
        self.request(
            Some(chat_id),
            "telegram.edit_message_reply_markup",
            || async {
                self.bot
                    .edit_message_reply_markup(chat_id, message_id)
                    .reply_markup(keyboard.clone())
                    .await
            },
        )
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
        self.request(Some(chat_id), "telegram.send_message", || async {
            self.bot
                .send_message(chat_id, message.to_owned())
                .parse_mode(ParseMode::Html)
                .reply_to(message_id)
                .await
        })
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
            .request(Some(chat_id), "telegram.send_media_group", || async {
                self.bot
                    .send_media_group(chat_id, media.clone())
                    .reply_to(message_id)
                    .await
            })
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
        self.request(Some(chat_id), "telegram.send_chat_action", || async {
            self.bot.send_chat_action(chat_id, action).await
        })
        .await?;
        Ok(())
    }

    async fn set_message_reaction(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        reaction: Option<ReactionType>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(Some(chat_id), "telegram.set_message_reaction", || async {
            self.bot
                .set_message_reaction(chat_id, message_id)
                .reaction(reaction.clone())
                .is_big(true)
                .await
        })
        .await?;
        Ok(())
    }

    async fn send_cached_video(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_id: &str,
        caption: &str,
    ) -> Result<MessageId, teloxide::RequestError> {
        log::info!("Sending cached video to chat {}", chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
        let msg = self
            .request(Some(chat_id), "telegram.send_cached_video", || async {
                self.bot
                    .send_video(chat_id, InputFile::file_id(file_id.to_owned().into()))
                    .caption(caption.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id)
                    .await
            })
            .await?;
        Ok(msg.id)
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
        self.request(Some(chat_id), "telegram.send_cached_photo", || async {
            self.bot
                .send_photo(chat_id, InputFile::file_id(file_id.to_owned().into()))
                .caption(caption.to_owned())
                .parse_mode(ParseMode::Html)
                .reply_to(message_id)
                .await
        })
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
                    caption.to_owned()
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
        self.request(
            Some(chat_id),
            "telegram.send_cached_media_group",
            || async {
                self.bot
                    .send_media_group(chat_id, media.clone())
                    .reply_to(message_id)
                    .await
            },
        )
        .await?;
        Ok(())
    }

    async fn send_audio(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        file_path: &std::path::Path,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending audio {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadDocument)
            .await?;
        self.request(Some(chat_id), "telegram.send_audio", || async {
            self.bot
                .send_audio(chat_id, InputFile::file(file_path))
                .reply_to(message_id)
                .await
        })
        .await?;
        Ok(())
    }

    async fn send_invoice(
        &self,
        chat_id: ChatId,
        title: &str,
        description: &str,
        payload: &str,
        price_amount: u32,
    ) -> Result<(), teloxide::RequestError> {
        use teloxide::types::LabeledPrice;
        self.request(Some(chat_id), "telegram.send_invoice", || async {
            self.bot
                .send_invoice(
                    chat_id,
                    title.to_owned(),
                    description.to_owned(),
                    payload.to_owned(),
                    "XTR",
                    vec![LabeledPrice::new(title.to_owned(), price_amount)],
                )
                .provider_token("")
                .await
        })
        .await?;
        Ok(())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<String>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(None, "telegram.answer_callback_query", || {
            let mut req = self
                .bot
                .answer_callback_query(teloxide::types::CallbackQueryId(
                    callback_query_id.to_string(),
                ));
            if let Some(t) = text.clone() {
                req = req.text(t);
            }
            async move { req.await }
        })
        .await?;
        Ok(())
    }

    async fn answer_pre_checkout_query(
        &self,
        pre_checkout_query_id: &str,
        ok: bool,
        error_message: Option<String>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(None, "telegram.answer_pre_checkout_query", || {
            let mut req = self.bot.answer_pre_checkout_query(
                teloxide::types::PreCheckoutQueryId(pre_checkout_query_id.to_string()),
                ok,
            );
            if let Some(msg) = error_message.clone() {
                req = req.error_message(msg);
            }
            async move { req.await }
        })
        .await?;
        Ok(())
    }

    async fn send_text_with_keyboard(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: &str,
        keyboard: InlineKeyboardMarkup,
    ) -> Result<(), teloxide::RequestError> {
        self.request(
            Some(chat_id),
            "telegram.send_text_with_keyboard",
            || async {
                self.bot
                    .send_message(chat_id, text.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id)
                    .reply_markup(keyboard.clone())
                    .await
            },
        )
        .await?;
        Ok(())
    }

    async fn send_text_no_reply(
        &self,
        chat_id: ChatId,
        text: &str,
    ) -> Result<(), teloxide::RequestError> {
        log::info!("Sending text (no reply) to chat {}", chat_id);
        self.request(Some(chat_id), "telegram.send_text_no_reply", || async {
            self.bot
                .send_message(chat_id, text.to_owned())
                .parse_mode(ParseMode::Html)
                .await
        })
        .await?;
        Ok(())
    }

    async fn refund_star_payment(
        &self,
        user_id: i64,
        telegram_payment_charge_id: &str,
    ) -> Result<(), teloxide::RequestError> {
        debug_assert!(user_id >= 0, "user_id must be non-negative");
        self.request(None, "telegram.refund_star_payment", || async {
            self.bot
                .refund_star_payment(
                    UserId(user_id as u64),
                    TelegramTransactionId(telegram_payment_charge_id.to_string()),
                )
                .await
        })
        .await?;
        Ok(())
    }
}
