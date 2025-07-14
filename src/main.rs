use log::LevelFilter;
use std::io::Write;
use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use url::Url;

// Use our library crate
use crabberbot::concurrency::ConcurrencyLimiter;
use crabberbot::downloader::{Downloader, YtDlpDownloader};
use crabberbot::handler::process_download_request;
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};

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
            let version_message = format!("CrabberBot version {0}", version);
            api.send_text_message(message.chat.id, message.id, &version_message)
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
    process_download_request(&url, chat_id, message.id, downloader.as_ref(), api.as_ref()).await;
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
}

#[tokio::main]
async fn main() {
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

    let bot = Bot::from_env();

    // Instantiate our REAL dependencies
    let downloader: Arc<dyn Downloader + Send + Sync> = Arc::new(YtDlpDownloader::new());
    let api: Arc<dyn TelegramApi + Send + Sync> = Arc::new(TeloxideApi::new(bot.clone()));
    let limiter = Arc::new(ConcurrencyLimiter::new());

    let addr = ([0, 0, 0, 0], 8080).into();
    let webhook_url_str = std::env::var("WEBHOOK_URL").expect("WEBHOOK_URL env var not set");
    let url: Url = webhook_url_str.parse().unwrap();

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
}
