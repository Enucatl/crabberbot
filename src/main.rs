use log::LevelFilter;
use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::postgres::PgPoolOptions;
use teloxide::prelude::*;
use teloxide::types::MessageKind;
use teloxide::utils::command::BotCommands;
use thiserror::Error;
use url::Url;

// Use our library crate
use crabberbot::commands::{
    handle_callback_query, handle_grant, handle_pre_checkout_query, handle_refund,
    handle_refunded_payment, handle_refundme, handle_reply, handle_subscribe,
    handle_successful_payment, handle_support,
};
use crabberbot::concurrency::ConcurrencyLimiter;
use crabberbot::config::AppConfig;
use crabberbot::downloader::{Downloader, YtDlpDownloader, cleanup_orphaned_downloads};
use crabberbot::handler::{maybe_send_premium_buttons, process_download_request};
use crabberbot::premium::audio_extractor::{AudioExtractor, FfmpegAudioExtractor};
use crabberbot::premium::summarizer::{GeminiSummarizer, Summarizer};
use crabberbot::premium::transcriber::{DeepgramTranscriber, Transcriber};
use crabberbot::storage::{PostgresStorage, Storage};
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};
use crabberbot::terms;

const OVERALL_REQUEST_TIMEOUT: Duration = Duration::from_secs(360);

/// A dedicated error type for our application's setup.
#[derive(Debug, Error)]
pub enum SetupError {
    #[error("Missing environment variable: {0}")]
    EnvVarMissing(&'static str),

    #[error("Couldn't get authentication headers: {0}")]
    HeadersError(&'static str),

    #[error("Failed to build Google Cloud authentication token")]
    BuildAuthError(#[from] google_cloud_auth::build_errors::Error),

    #[error("Failed to acquire Google Cloud authentication token")]
    CredentialAuthError(#[from] google_cloud_auth::errors::CredentialsError),

    #[error("Failed to build HTTP client")]
    ClientBuildError(#[from] reqwest::Error),

    #[error("The Authorization header value could not be created")]
    InvalidHeaderValue(#[from] reqwest::header::InvalidHeaderValue),
}

/// Creates an HTTP client, authenticating for GCP if in that environment.
async fn create_http_client(execution_environment: &str) -> Result<Client, SetupError> {
    match execution_environment {
        "gcp" => {
            log::info!("GCP environment detected. Creating authenticated reqwest client...");

            let credentials = google_cloud_auth::credentials::Builder::default().build()?;
            let headers_resource = credentials.headers(http::Extensions::new()).await?;
            if let google_cloud_auth::credentials::CacheableResource::New {
                data: headers, ..
            } = headers_resource
            {
                log::info!(
                    "Successfully obtained GCP authentication headers. {:?}",
                    headers
                );
                let client = Client::builder().default_headers(headers).build()?;
                Ok(client)
            } else {
                Err(SetupError::HeadersError(
                    "Failed to get new headers from credentials; received NotModified unexpectedly",
                ))
            }
        }
        _ => {
            log::info!(
                "Local or non-GCP environment detected. Creating standard reqwest client..."
            );
            Ok(Client::new())
        }
    }
}

async fn handle_command(
    _bot: Bot,
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
    command: Command,
    owner_chat_id: i64,
    execution_environment: String,
) -> ResponseResult<()> {
    log_update_context("command", &message);
    let comprehensive_guide = indoc::formatdoc! { "
Hello there! I am CrabberBot, your friendly media downloader.

I can download videos and photos from various platforms like Instagram, TikTok, YouTube Shorts, and many more!

<b>How to use me</b>
To download media, simply send me the URL of the media you want to download.
Example: <code>https://www.youtube.com/shorts/tPEE9ZwTmy0</code>

I'll try my best to fetch the media and send it back to you. I also include the original caption (limited to 1024 characters).
If you encounter any issues, please double-check the URL or try again later. Not all links may be supported, or there might be temporary issues.

{0}
",
        Command::descriptions()
    };

    match command {
        Command::Start => {
            api.send_text_message(message.chat.id, message.id, &comprehensive_guide)
                .await?;
        }
        Command::Version => {
            let version = env!("CARGO_PACKAGE_VERSION");
            let value = format!("CrabberBot version {0}", version);
            api.send_text_message(message.chat.id, message.id, &value)
                .await?;
        }
        Command::Environment => {
            let value = format!("CrabberBot environment {0}", execution_environment);
            api.send_text_message(message.chat.id, message.id, &value)
                .await?;
        }
        Command::Subscribe => {
            handle_subscribe(api, message, storage).await?;
        }
        Command::Terms => {
            api.send_text_message(message.chat.id, message.id, &terms::terms_text())
                .await?;
        }
        Command::Support(text) => {
            handle_support(api, storage, message, text, owner_chat_id).await?;
        }
        Command::Refundme => {
            handle_refundme(api, storage, message).await?;
        }
    }

    Ok(())
}

async fn handle_owner_command(
    _bot: Bot,
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
    command: OwnerCommand,
    owner_chat_id: i64,
) -> ResponseResult<()> {
    log_update_context("owner_command", &message);
    match command {
        OwnerCommand::Grant(args) => {
            handle_grant(api, message, storage, args, owner_chat_id).await?
        }
        OwnerCommand::Reply(args) => handle_reply(api, message, args, owner_chat_id).await?,
        OwnerCommand::Refund(args) => {
            handle_refund(api, storage, message, args, owner_chat_id).await?
        }
    }
    Ok(())
}

async fn handle_url(
    _bot: Bot,
    downloader: Arc<dyn Downloader>,
    api: Arc<dyn TelegramApi>,
    download_limiter: Arc<ConcurrencyLimiter>,
    storage: Arc<dyn Storage>,
    audio_extractor: Arc<dyn AudioExtractor>,
    message: Message,
    url: Url,
) -> ResponseResult<()> {
    let chat_id = message.chat.id;
    log::info!(
        "request_context action=url update_message_id={} chat_id={} user_id={:?} url={}",
        message.id,
        chat_id,
        message.from.as_ref().map(|user| user.id.0),
        url
    );

    let _guard = match download_limiter.try_lock(chat_id) {
        Some(guard) => guard,
        None => {
            api.send_text_message(
                chat_id,
                message.id,
                "I'm already working on a request for you. Please wait until it's finished!",
            )
            .await?;
            return Ok(());
        }
    };
    api.send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
        .await?;
    api.set_message_reaction(
        chat_id,
        message.id,
        Some(teloxide::types::ReactionType::Emoji {
            emoji: "👀".to_string(),
        }),
    )
    .await?;

    let result = tokio::time::timeout(
        OVERALL_REQUEST_TIMEOUT,
        process_download_request(
            &url,
            chat_id,
            message.id,
            downloader.as_ref(),
            api.as_ref(),
            storage.as_ref(),
            audio_extractor.as_ref(),
        ),
    )
    .await;

    let download_ctx = match result {
        Err(_) => {
            log::error!("Overall request timed out for {}", url);
            if let Err(e) = api
                .send_text_message(
                    chat_id,
                    message.id,
                    "Sorry, the request timed out. Please try again.",
                )
                .await
            {
                log::error!(
                    "Telegram reply failed: action=request_timeout chat_id={} error={:?}",
                    chat_id,
                    e
                );
            }
            None
        }
        Ok(ctx) => ctx,
    };

    api.set_message_reaction(chat_id, message.id, None).await?;

    // Send premium buttons if we have a download context with video + cached audio
    if let Some(ctx) = download_ctx {
        maybe_send_premium_buttons(chat_id, ctx, &*api, &*storage).await;
    }

    Ok(())
}

fn log_update_context(action: &str, message: &Message) {
    log::info!(
        "request_context action={} update_message_id={} chat_id={} user_id={:?}",
        action,
        message.id,
        message.chat.id,
        message.from.as_ref().map(|user| user.id.0)
    );
}

// Required catch-all branch — silently ignore messages that are neither commands nor URLs.
async fn handle_unhandled_message(
    _bot: Bot,
    _downloader: Arc<dyn Downloader>,
    _api: Arc<dyn TelegramApi>,
    _message: Message,
) -> ResponseResult<()> {
    Ok(())
}

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "start interaction and display a guide.")]
    Start,
    #[command(description = "show bot version.")]
    Version,
    #[command(description = "show bot environment.")]
    Environment,
    #[command(description = "subscribe or buy AI Video Minutes top-up.")]
    Subscribe,
    #[command(description = "view Terms of Service.")]
    Terms,
    #[command(description = "contact customer support or get help with a payment issue.")]
    Support(String),
    #[command(description = "request a refund for your most recent purchase.")]
    Refundme,
}

/// Owner-only commands. Never registered with Telegram (no autocomplete),
/// handled in a separate dptree branch that pre-filters on owner chat_id.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum OwnerCommand {
    Grant(String),
    Reply(String),
    Refund(String),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = pretty_env_logger::formatted_builder();

    builder.filter_level(LevelFilter::Info);

    builder.format(|buf, record| {
        writeln!(
            buf,
            "{} | {} | {}:{} | {}",
            buf.timestamp(),
            record.level(),
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.args()
        )
    });

    builder.init();

    let version = env!("CARGO_PACKAGE_VERSION");
    log::info!("Starting CrabberBot version {}", version);

    let config = AppConfig::from_env()?;
    if config.deepgram_api_key.is_empty() || config.gemini_api_key.is_empty() {
        log::warn!(
            "DEEPGRAM_API_KEY and/or GEMINI_API_KEY not set — transcription and summarization will be unavailable"
        );
    }
    log::info!(
        "Postgres pool configured: min={} max={} acquire_timeout={:?}",
        config.postgres_min_connections,
        config.postgres_max_connections,
        config.postgres_acquire_timeout
    );

    let removed_orphans = cleanup_orphaned_downloads(&config.downloads_dir).await;
    if removed_orphans > 0 {
        log::info!(
            "Startup cleanup removed {} orphaned download artifact(s)",
            removed_orphans
        );
    }

    let pool = PgPoolOptions::new()
        .max_connections(config.postgres_max_connections)
        .min_connections(config.postgres_min_connections)
        .acquire_timeout(config.postgres_acquire_timeout)
        .connect(&config.database_url)
        .await
        .expect("Failed to connect to database");
    PostgresStorage::run_migrations(&pool)
        .await
        .expect("Failed to run database migrations");
    log::info!("Database connected and migrations applied.");
    let storage: Arc<dyn Storage> = Arc::new(PostgresStorage::new(pool.clone()));

    let audio_cache_dir = config.audio_cache_dir.clone();
    let cleanup_pool = pool.clone();
    let cleanup_storage = storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            interval.tick().await;
            PostgresStorage::cleanup_expired(&cleanup_pool, 7).await;
            cleanup_storage.cleanup_expired_callback_contexts().await;
            cleanup_storage.expire_stale_topups().await;
            cleanup_audio_cache(&cleanup_pool, &audio_cache_dir).await;
        }
    });

    let client = create_http_client(&config.execution_environment).await?;
    let bot = Bot::from_env_with_client(client.clone());

    let downloader: Arc<dyn Downloader> = Arc::new(
        YtDlpDownloader::new(config.yt_dlp_path.clone(), config.downloads_dir.clone()).await,
    );
    let api: Arc<dyn TelegramApi> = Arc::new(TeloxideApi::new(bot.clone()));
    let download_limiter = Arc::new(ConcurrencyLimiter::new());
    let premium_limiter = Arc::new(ConcurrencyLimiter::new());
    let audio_extractor: Arc<dyn AudioExtractor> =
        Arc::new(FfmpegAudioExtractor::new(3, config.audio_cache_dir.clone()));
    let transcriber: Arc<dyn Transcriber> = Arc::new(DeepgramTranscriber::new(
        client.clone(),
        config.deepgram_api_key.clone(),
    ));
    let summarizer: Arc<dyn Summarizer> = Arc::new(GeminiSummarizer::new(
        client.clone(),
        config.gemini_api_key.clone(),
        config.gemini_model.clone(),
    ));

    let addr = ([0, 0, 0, 0], config.port).into();
    let url = config.webhook_url.clone();

    log::info!("Setting webhook {}", url);
    let listener = teloxide::update_listeners::webhooks::axum(
        bot.clone(),
        teloxide::update_listeners::webhooks::Options::new(addr, url.clone()),
    )
    .await
    .expect("Failed to set webhook");
    log::info!("Successfully set webhook {}", url);

    bot.set_my_commands(Command::bot_commands())
        .await
        .expect("Failed to set bot commands.");
    log::info!("Successfully set bot commands.");

    let bot_description = "Your friendly media downloader from various platforms like Instagram, TikTok, YouTube, and more!";
    bot.set_my_description()
        .description(bot_description)
        .await
        .expect("Failed to set bot description.");
    log::info!("Successfully set bot description.");

    let bot_name = if config.webhook_url.as_str().contains("test") {
        "CrabberBot TEST"
    } else {
        "CrabberBot | Video Downloader"
    };
    log::info!("Successfully set bot name. {}", bot_name);

    let successful_payment_filter =
        dptree::filter(|msg: Message| msg.successful_payment().is_some());
    let refunded_payment_filter =
        dptree::filter(|msg: Message| matches!(msg.kind, MessageKind::RefundedPayment(_)));

    let owner_commands = dptree::entry()
        .filter(|msg: Message, oid: i64| msg.chat.id.0 == oid)
        .filter_command::<OwnerCommand>()
        .endpoint(handle_owner_command);
    let commands = dptree::entry()
        .filter_command::<Command>()
        .endpoint(handle_command);
    let urls = dptree::entry()
        .filter_map(|msg: Message| msg.text().and_then(|text| Url::parse(text).ok()))
        .endpoint(handle_url);

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .branch(
                    successful_payment_filter
                        .endpoint(|api: Arc<dyn TelegramApi>, storage: Arc<dyn Storage>, msg: Message| async move {
                            handle_successful_payment(api, storage, msg).await
                        }),
                )
                .branch(
                    refunded_payment_filter
                        .endpoint(|api: Arc<dyn TelegramApi>, storage: Arc<dyn Storage>, msg: Message| async move {
                            handle_refunded_payment(api, storage, msg).await
                        }),
                )
                .branch(owner_commands)
                .branch(commands)
                .branch(urls)
                .branch(dptree::entry().endpoint(handle_unhandled_message)),
        )
        .branch(
            Update::filter_callback_query().endpoint(handle_callback_query),
        )
        .branch(
            Update::filter_pre_checkout_query().endpoint(handle_pre_checkout_query),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            downloader,
            api,
            download_limiter,
            premium_limiter,
            storage,
            audio_extractor,
            transcriber,
            summarizer,
            config.owner_chat_id,
            config.execution_environment.clone()
        ])
        .enable_ctrlc_handler()
        .build()
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error has occurred in the dispatcher"),
        )
        .await;

    Ok(())
}

/// Delete audio cache files older than 2 hours.
async fn cleanup_audio_cache(pool: &sqlx::PgPool, audio_cache_dir: &std::path::Path) {
    // Fetch paths currently referenced by active (non-expired) cache entries so
    // we don't delete audio files that are still needed for premium buttons.
    let referenced: HashSet<String> = sqlx::query_as::<_, (String,)>(
        "SELECT audio_cache_path FROM media_cache WHERE audio_cache_path IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(p,)| p)
    .collect();

    let mut entries = match tokio::fs::read_dir(audio_cache_dir).await {
        Ok(e) => e,
        Err(e) => {
            log::warn!("Failed to read audio cache dir: {}", e);
            return;
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let path = entry.path();
                let path_str = path.to_string_lossy();
                if referenced.contains(path_str.as_ref()) {
                    continue; // live cache entry — leave it alone
                }
                if let Ok(metadata) = entry.metadata().await {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default() > Duration::from_secs(7200) {
                            let _ = tokio::fs::remove_file(&path).await;
                            log::info!("Removed orphaned audio cache: {:?}", path);
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::warn!("Error reading audio cache entry: {}", e);
                break;
            }
        }
    }
}
