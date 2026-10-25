use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use image::GenericImageView;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::{
    prelude::*,
    types::{
        ChatAction, ChatId, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaPhoto,
        InputMediaVideo, MessageId, ParseMode, ReactionType, TelegramTransactionId, UserId,
    },
};
use tokio::sync::{Mutex, Semaphore};

use crate::downloader::MediaType;
use crate::retry::{RetryPolicy, retry_async};
use crate::storage::CachedFile;

static IMAGE_JOBS: Semaphore = Semaphore::const_new(2);

const TELEGRAM_MAX_DIMENSION_SUM: u32 = 10_000;
const MAX_PHOTO_WIDTH: u32 = 12_000;
const MAX_PHOTO_HEIGHT: u32 = 12_000;
const MAX_PHOTO_PIXELS: u64 = 48_000_000;
const MAX_PHOTO_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_VIDEO_THUMBNAIL_DIMENSION: u32 = 320;
const MAX_VIDEO_THUMBNAIL_BYTES: u64 = 200_000;

pub(crate) struct TempImage(PathBuf);

impl TempImage {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempImage {
    fn drop(&mut self) {
        let path = self.0.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = tokio::fs::remove_file(&path).await
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("Could not remove temporary image {:?}: {}", path, e);
                }
            });
        } else if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Could not remove temporary image {:?}: {}", path, e);
        }
    }
}

fn prepare_video_thumbnail(path: &Path) -> Result<TempImage, String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| e.to_string())
        .and_then(|reader| reader.with_guessed_format().map_err(|e| e.to_string()))
        .and_then(|mut reader| {
            reader.limits(image_limits());
            reader.decode().map_err(|e| e.to_string())
        })?;
    let resized = img.thumbnail(MAX_VIDEO_THUMBNAIL_DIMENSION, MAX_VIDEO_THUMBNAIL_DIMENSION);
    let temp_path = TempImage(std::env::temp_dir().join(format!("{}.jpg", uuid::Uuid::new_v4())));
    let file = std::fs::File::create(temp_path.path()).map_err(|e| e.to_string())?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(file, 80)
        .encode_image(&resized)
        .map_err(|e| e.to_string())?;
    let size = std::fs::metadata(temp_path.path())
        .map_err(|e| e.to_string())?
        .len();
    if size >= MAX_VIDEO_THUMBNAIL_BYTES {
        return Err(format!("encoded thumbnail is {size} bytes"));
    }
    Ok(temp_path)
}

/// Resize a photo if its dimension sum exceeds Telegram's 10000 limit.
/// Returns the path to a temporary resized file, or None if no resize was needed.
/// The returned guard deletes a resized file when dropped.
fn resize_photo_if_needed_sync(path: &Path) -> Result<Option<TempImage>, String> {
    let dimensions = match image::ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(image::ImageError::IoError)
        .and_then(|mut reader| {
            reader.limits(image_limits());
            reader.into_dimensions()
        }) {
        Ok(dimensions) => dimensions,
        Err(image::ImageError::Limits(error)) => {
            log::warn!("Rejecting photo {:?}: {}", path, error);
            return Err("Photo exceeds the configured image safety limits.".to_string());
        }
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
    let temp_path =
        TempImage(std::env::temp_dir().join(format!("{}.{}", uuid::Uuid::new_v4(), ext)));
    if let Err(e) = resized.save(temp_path.path()) {
        log::warn!(
            "Could not save resized image to {:?}: {}",
            temp_path.path(),
            e
        );
        return Ok(None);
    }
    Ok(Some(temp_path))
}

pub(crate) async fn resize_photo_if_needed(path: PathBuf) -> Result<Option<TempImage>, String> {
    process_image(move || resize_photo_if_needed_sync(&path)).await
}

async fn process_image<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let permit = IMAGE_JOBS.acquire().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        // Keep the permit until CPU work finishes even if its caller is cancelled.
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|e| e.to_string())?
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

#[derive(Clone, Copy)]
enum RetrySafety {
    Idempotent,
    NonIdempotent,
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
        safety: RetrySafety,
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
                matches!(error, teloxide::RequestError::RetryAfter(_))
                    || (matches!(safety, RetrySafety::Idempotent)
                        && matches!(
                            error,
                            teloxide::RequestError::Network(_)
                                | teloxide::RequestError::InvalidJson { .. }
                        ))
            },
            label,
        )
        .await
    }
}

struct TelegramRequestLimiter {
    state: Mutex<LimiterState>,
}

struct LimiterState {
    global_next: Instant,
    chat_next: HashMap<i64, Instant>,
    last_prune: Instant,
}

impl TelegramRequestLimiter {
    fn new() -> Self {
        Self {
            state: Mutex::new(LimiterState {
                global_next: Instant::now(),
                chat_next: HashMap::new(),
                last_prune: Instant::now(),
            }),
        }
    }

    async fn wait(&self, chat_id: Option<ChatId>) {
        loop {
            let mut state = self.state.lock().await;
            let now = Instant::now();
            if now.duration_since(state.last_prune) >= Duration::from_secs(60) {
                state.chat_next.retain(|_, next| {
                    now.saturating_duration_since(*next) < Duration::from_secs(600)
                });
                state.last_prune = now;
            }
            let chat_next = chat_id.and_then(|id| state.chat_next.get(&id.0).copied());
            let ready = state.global_next.max(chat_next.unwrap_or(now));
            if ready <= now {
                state.global_next = now + Duration::from_millis(34);
                if let Some(id) = chat_id {
                    state
                        .chat_next
                        .insert(id.0, now + Duration::from_millis(1_100));
                }
                return;
            }
            drop(state);
            tokio::time::sleep(ready - now).await;
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
    ) -> Result<(String, MessageId), teloxide::RequestError> {
        log::info!("Sending video {:?} to chat {}", file_path, chat_id);
        self.send_chat_action(chat_id, ChatAction::UploadVideo)
            .await?;
        let prepared_thumbnail = if let Some(path) = thumbnail_filepath {
            process_image({
                let path = path.clone();
                move || prepare_video_thumbnail(&path)
            })
            .await
            .inspect_err(|e| log::warn!("Could not prepare video thumbnail {:?}: {}", path, e))
            .ok()
        } else {
            None
        };
        let result = self
            .request(
                Some(chat_id),
                "telegram.send_video",
                RetrySafety::NonIdempotent,
                || {
                    let mut request = self
                        .bot
                        .send_video(chat_id, InputFile::file(file_path))
                        .caption(caption.to_owned())
                        .parse_mode(ParseMode::Html)
                        .reply_to(message_id);

                    if let Some(p) = &prepared_thumbnail {
                        request = request.thumbnail(InputFile::file(p.path()));
                    }
                    async move { request.await }
                },
            )
            .await;
        let message = result?;
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
            .request(
                Some(chat_id),
                "telegram.send_photo",
                RetrySafety::NonIdempotent,
                || async {
                    self.bot
                        .send_photo(chat_id, InputFile::file(file_path))
                        .caption(caption.to_owned())
                        .parse_mode(ParseMode::Html)
                        .reply_to(message_id)
                        .await
                },
            )
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
            RetrySafety::Idempotent,
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
        self.request(
            Some(chat_id),
            "telegram.send_message",
            RetrySafety::NonIdempotent,
            || async {
                self.bot
                    .send_message(chat_id, message.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id)
                    .await
            },
        )
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
            .request(
                Some(chat_id),
                "telegram.send_media_group",
                RetrySafety::NonIdempotent,
                || async {
                    self.bot
                        .send_media_group(chat_id, media.clone())
                        .reply_to(message_id)
                        .await
                },
            )
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
        self.request(
            Some(chat_id),
            "telegram.send_chat_action",
            RetrySafety::Idempotent,
            || async { self.bot.send_chat_action(chat_id, action).await },
        )
        .await?;
        Ok(())
    }

    async fn set_message_reaction(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        reaction: Option<ReactionType>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(
            Some(chat_id),
            "telegram.set_message_reaction",
            RetrySafety::Idempotent,
            || async {
                self.bot
                    .set_message_reaction(chat_id, message_id)
                    .reaction(reaction.clone())
                    .is_big(true)
                    .await
            },
        )
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
            .request(
                Some(chat_id),
                "telegram.send_cached_video",
                RetrySafety::NonIdempotent,
                || async {
                    self.bot
                        .send_video(chat_id, InputFile::file_id(file_id.to_owned().into()))
                        .caption(caption.to_owned())
                        .parse_mode(ParseMode::Html)
                        .reply_to(message_id)
                        .await
                },
            )
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
        self.request(
            Some(chat_id),
            "telegram.send_cached_photo",
            RetrySafety::NonIdempotent,
            || async {
                self.bot
                    .send_photo(chat_id, InputFile::file_id(file_id.to_owned().into()))
                    .caption(caption.to_owned())
                    .parse_mode(ParseMode::Html)
                    .reply_to(message_id)
                    .await
            },
        )
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
            RetrySafety::NonIdempotent,
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
        self.request(
            Some(chat_id),
            "telegram.send_audio",
            RetrySafety::NonIdempotent,
            || async {
                self.bot
                    .send_audio(chat_id, InputFile::file(file_path))
                    .reply_to(message_id)
                    .await
            },
        )
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
        self.request(
            Some(chat_id),
            "telegram.send_invoice",
            RetrySafety::NonIdempotent,
            || async {
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
            },
        )
        .await?;
        Ok(())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<String>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(
            None,
            "telegram.answer_callback_query",
            RetrySafety::NonIdempotent,
            || {
                let mut req = self
                    .bot
                    .answer_callback_query(teloxide::types::CallbackQueryId(
                        callback_query_id.to_string(),
                    ));
                if let Some(t) = text.clone() {
                    req = req.text(t);
                }
                async move { req.await }
            },
        )
        .await?;
        Ok(())
    }

    async fn answer_pre_checkout_query(
        &self,
        pre_checkout_query_id: &str,
        ok: bool,
        error_message: Option<String>,
    ) -> Result<(), teloxide::RequestError> {
        self.request(
            None,
            "telegram.answer_pre_checkout_query",
            RetrySafety::NonIdempotent,
            || {
                let mut req = self.bot.answer_pre_checkout_query(
                    teloxide::types::PreCheckoutQueryId(pre_checkout_query_id.to_string()),
                    ok,
                );
                if let Some(msg) = error_message.clone() {
                    req = req.error_message(msg);
                }
                async move { req.await }
            },
        )
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
            RetrySafety::NonIdempotent,
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
        self.request(
            Some(chat_id),
            "telegram.send_text_no_reply",
            RetrySafety::NonIdempotent,
            || async {
                self.bot
                    .send_message(chat_id, text.to_owned())
                    .parse_mode(ParseMode::Html)
                    .await
            },
        )
        .await?;
        Ok(())
    }

    async fn refund_star_payment(
        &self,
        user_id: i64,
        telegram_payment_charge_id: &str,
    ) -> Result<(), teloxide::RequestError> {
        debug_assert!(user_id >= 0, "user_id must be non-negative");
        self.request(
            None,
            "telegram.refund_star_payment",
            RetrySafety::NonIdempotent,
            || async {
                self.bot
                    .refund_star_payment(
                        UserId(user_id as u64),
                        TelegramTransactionId(telegram_payment_charge_id.to_string()),
                    )
                    .await
            },
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retries_ambiguous_failures_only_for_idempotent_operations() {
        for (safety, expected) in [
            (RetrySafety::NonIdempotent, 1),
            (RetrySafety::Idempotent, 2),
        ] {
            let mut api = TeloxideApi::new(Bot::new("test"));
            api.retry_policy.max_attempts = 2;
            api.retry_policy.base_delay = Duration::ZERO;
            api.retry_policy.max_delay = Duration::ZERO;
            let mut attempts = 0;
            let result: Result<(), _> = api
                .request(None, "test", safety, || {
                    attempts += 1;
                    async {
                        Err(teloxide::RequestError::InvalidJson {
                            source: Arc::new(
                                serde_json::from_str::<serde_json::Value>("invalid").unwrap_err(),
                            ),
                            raw: "invalid".into(),
                        })
                    }
                })
                .await;
            assert!(result.is_err());
            assert_eq!(attempts, expected);
        }
    }

    #[tokio::test]
    async fn retries_explicit_rate_limit_rejections_for_sends() {
        let mut api = TeloxideApi::new(Bot::new("test"));
        api.retry_policy.base_delay = Duration::ZERO;
        let mut attempts = 0;
        let result = api
            .request(None, "test", RetrySafety::NonIdempotent, || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt == 1 {
                        Err(teloxide::RequestError::RetryAfter(
                            teloxide::types::Seconds::from_seconds(0),
                        ))
                    } else {
                        Ok(())
                    }
                }
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn chat_waiters_recheck_global_spacing_when_they_become_ready() {
        let limiter = TelegramRequestLimiter::new();
        {
            let mut state = limiter.state.lock().await;
            let ready = Instant::now() + Duration::from_millis(100);
            state.chat_next.insert(1, ready);
            state.chat_next.insert(2, ready);
        }
        let (first, second) = tokio::join!(
            async {
                limiter.wait(Some(ChatId(1))).await;
                Instant::now()
            },
            async {
                limiter.wait(Some(ChatId(2))).await;
                Instant::now()
            },
        );
        let spacing = first.max(second).duration_since(first.min(second));
        assert!(
            spacing >= Duration::from_millis(30),
            "spacing was {spacing:?}"
        );
    }

    #[tokio::test]
    async fn chat_spacing_is_preserved_without_blocking_other_chats() {
        let limiter = TelegramRequestLimiter::new();
        limiter.wait(Some(ChatId(1))).await;
        let first = Instant::now();
        let (same_chat, other_chat) = tokio::join!(
            async {
                limiter.wait(Some(ChatId(1))).await;
                Instant::now()
            },
            async {
                limiter.wait(Some(ChatId(2))).await;
                Instant::now()
            },
        );
        assert!(same_chat.duration_since(first) >= Duration::from_millis(1_090));
        assert!(other_chat < same_chat);
    }

    #[tokio::test]
    async fn cancelled_image_caller_keeps_permit_until_blocking_work_finishes() {
        let reserved = IMAGE_JOBS.acquire().await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let job = tokio::spawn(process_image(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert!(IMAGE_JOBS.try_acquire().is_err());
        finish_tx.send(()).unwrap();
        let released = tokio::time::timeout(Duration::from_secs(10), IMAGE_JOBS.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(released);
        drop(reserved);
    }

    #[test]
    fn video_thumbnail_meets_telegram_limits() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        image::DynamicImage::new_rgb8(1320, 2347)
            .save(&source)
            .unwrap();

        let prepared = prepare_video_thumbnail(&source).unwrap();
        let (width, height) = image::ImageReader::open(prepared.path())
            .unwrap()
            .into_dimensions()
            .unwrap();

        assert!(width <= 320 && height <= 320);
        assert!(std::fs::metadata(prepared.path()).unwrap().len() < 200_000);
        assert_eq!(prepared.path().extension().unwrap(), "jpg");
        let path = prepared.path().to_path_buf();
        drop(prepared);
        assert!(!path.exists());
    }

    #[test]
    fn temporary_image_removes_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("partial.jpg");
        let write_result: std::io::Result<TempImage> = (|| {
            let temporary = TempImage(output.clone());
            std::fs::write(temporary.path(), b"partial JPEG data")?;
            Err(std::io::Error::other("encoding failed"))
        })();
        assert!(write_result.is_err());
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn cancelled_upload_removes_resized_photo() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        image::DynamicImage::new_rgb8(2, 9999)
            .save(&source)
            .unwrap();

        let (path_tx, path_rx) = tokio::sync::oneshot::channel();
        let upload = tokio::spawn(async move {
            let resized = resize_photo_if_needed(source).await.unwrap().unwrap();
            path_tx.send(resized.path().to_path_buf()).unwrap();
            std::future::pending::<()>().await;
            drop(resized);
        });

        let path = path_rx.await.unwrap();
        assert!(path.exists());
        upload.abort();
        assert!(upload.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::fs::try_exists(&path).await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn abandoned_blocking_transform_removes_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("partial.jpg");
        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let transform = tokio::task::spawn_blocking({
            let output = output.clone();
            move || {
                let temporary = TempImage(output.clone());
                std::fs::write(&output, b"partial JPEG data").unwrap();
                created_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                temporary
            }
        });

        created_rx.await.unwrap();
        assert!(output.exists());
        drop(transform);
        finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while output.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn photo_resize_uses_blocking_pool() {
        use std::future::Future;
        use std::task::Poll;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        image::DynamicImage::new_rgb8(2, 9999)
            .save(&source)
            .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        runtime.block_on(async {
            let resize = resize_photo_if_needed(source);
            tokio::pin!(resize);
            let pending =
                std::future::poll_fn(|cx| Poll::Ready(resize.as_mut().poll(cx).is_pending())).await;
            finish_tx.send(()).unwrap();
            assert!(pending, "resizing must wait for the occupied blocking pool");
            blocker.await.unwrap();
            let resized = resize.await.unwrap().unwrap();
            assert!(resized.path().exists());
        });
    }

    #[tokio::test]
    async fn prunes_only_idle_chat_limiters() {
        let limiter = TelegramRequestLimiter::new();
        {
            let mut state = limiter.state.lock().await;
            state
                .chat_next
                .insert(1, Instant::now() - Duration::from_secs(601));
            state.chat_next.insert(2, Instant::now());
            state.last_prune = Instant::now() - Duration::from_secs(61);
        }
        limiter.wait(None).await;
        let state = limiter.state.lock().await;
        assert!(!state.chat_next.contains_key(&1));
        assert!(state.chat_next.contains_key(&2));
    }
}
