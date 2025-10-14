use log::LevelFilter;
use std::io::Write;
use std::sync::Arc;

use reqwest::Client;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use thiserror::Error;
use url::Url;

// Use our library crate
use crabberbot::concurrency::ConcurrencyLimiter;
use crabberbot::downloader::{Downloader, YtDlpDownloader};
use crabberbot::handler::process_download_request;
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};

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
///
/// # Errors
/// This function will return an error if:
/// - A required environment variable is missing in the GCP environment.
/// - It fails to get an identity token from Google Cloud.
/// - It fails to build the reqwest::Client.
async fn create_http_client() -> Result<Client, SetupError> {
    // Determine the execution environment. Default to "local" if not set.
    let exec_env = std::env::var("EXECUTION_ENVIRONMENT").unwrap_or_else(|_| "local".to_string());

    match exec_env.as_str() {
        "gcp" => {
            log::info!("GCP environment detected. Creating authenticated reqwest client...");

            let credentials = google_cloud_auth::credentials::Builder::default().build()?;
            let headers_resource = credentials.headers(http::Extensions::new()).await?;
            if let google_cloud_auth::credentials::CacheableResource::New {
                data: headers, ..
            } = headers_resource
            {
                log::info!("Successfully obtained GCP authentication headers. {:?}", headers);
                let client = Client::builder().default_headers(headers).build()?;
                Ok(client)
            } else {
                // This case should be logically impossible when fetching headers for the first time,
                // but it's robust to handle it.
                Err(SetupError::HeadersError(
                    "Failed to get new headers from credentials; received NotModified unexpectedly"
                ))
            }
        }
        _ => {
            // "local", "homelab", or any other value
            log::info!(
                "Local or non-GCP environment detected. Creating standard reqwest client..."
            );
            // Building a default client can also fail, so we handle the result here too.
            Ok(Client::new())
        }
    }
}

async fn handle_command(
    _bot: Bot,
    api: Arc<dyn TelegramApi + Send + Sync>,
    message: Message,
    command: Command,
) -> ResponseResult<()> {
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
            // Send the comprehensive guide message for /start
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
            let env = std::env::var("EXECUTION_ENVIRONMENT").unwrap_or_else(|_| "local".to_string());
            let value = format!("CrabberBot environment {0}", env);
            api.send_text_message(message.chat.id, message.id, &value)
                .await?;
        }
    }

    Ok(())
}

async fn handle_url(
    _bot: Bot,
    downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
    limiter: Arc<ConcurrencyLimiter>,
    message: Message,
    url: Url,
) -> ResponseResult<()> {
    let chat_id = message.chat.id;

    // --- CONCURRENCY CHECK ---
    let _guard = match limiter.try_lock(chat_id) {
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
    process_download_request(&url, chat_id, message.id, downloader.as_ref(), api.as_ref()).await;
    api.set_message_reaction(chat_id, message.id, None).await?;
    Ok(())
}

async fn handle_unhandled_message(
    _bot: Bot,
    _downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
    message: Message,
) -> ResponseResult<()> {
    api.send_text_message(
        message.chat.id,
        message.id,
        "Your message isn't a valid link!",
    )
    .await?;
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = pretty_env_logger::formatted_builder();

    // Set a default log level if RUST_LOG is not set.
    builder.filter_level(LevelFilter::Info);

    // Define and apply the custom format.
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

    // Initialize the logger.
    builder.init();

    let version = env!("CARGO_PACKAGE_VERSION");
    log::info!("Starting CrabberBot version {}", version);

    let client = create_http_client().await?;
    let bot = Bot::from_env_with_client(client);

    // Instantiate our REAL dependencies
    let downloader: Arc<dyn Downloader + Send + Sync> = Arc::new(YtDlpDownloader::new());
    let api: Arc<dyn TelegramApi + Send + Sync> = Arc::new(TeloxideApi::new(bot.clone()));
    let limiter = Arc::new(ConcurrencyLimiter::new());

    // Get port from environment, fallback to 8080 for local development
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let addr = ([0, 0, 0, 0], port).into();
    let webhook_url_str = std::env::var("WEBHOOK_URL").expect("WEBHOOK_URL env var not set");
    let url: Url = webhook_url_str.parse().unwrap();

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

    let mut bot_name = String::from("CrabberBot | Video Downloader");
    if webhook_url_str.contains("test") {
        bot_name = String::from("CrabberBot TEST");
    }
    // bot.set_my_name()
    //     .name(bot_name.clone())
    //     .await
    //     .expect("Failed to set bot name.");
    log::info!("Successfully set bot name. {}", bot_name);

    let handler = Update::filter_message()
        .branch(
            dptree::entry()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(
            dptree::entry()
                // Get the text, then try to parse it as a URL.
                // .filter_map returns Some(Url) if successful, None if not,
                .filter_map(|msg: Message| msg.text().and_then(|text| Url::parse(text).ok()))
                .endpoint(handle_url),
        )
        // Handler for any message type not caught by the above branches
        .branch(dptree::entry().endpoint(handle_unhandled_message));

    // The dispatcher will inject the dependencies into our handler
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![downloader, api, limiter])
        .enable_ctrlc_handler()
        .build()
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error has occurred in the dispatcher"),
        )
        .await;

    Ok(())
}
