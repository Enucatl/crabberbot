use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

// Use our library crate
use crabberbot::downloader::{Downloader, YtDlpDownloader};
use crabberbot::handler::process_download_request;
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};

async fn handle_command(
    bot: Bot,
    downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
    message: Message,
    command: Command,
) -> ResponseResult<()> {
    // Acknowledge the request for better UX
    bot.send_chat_action(message.chat.id, teloxide::types::ChatAction::Typing)
        .await?;

    // Define the comprehensive guide message
    let comprehensive_guide = indoc::formatdoc! { "
        Hello there! I am CrabberBot, your friendly media downloader.

        I can download videos and photos from various platforms like Instagram, TikTok, YouTube Shorts, and many more!

        <b>How to use me</b>
        To download media, simply send me the <code>/download</code> command followed by the URL of the media you want to download.
        Example: <code>/download https://www.youtube.com/shorts/tPEE9ZwTmy0</code>

        I'll try my best to fetch the media and send it back to you. I also include the original caption (limited to 1024 characters).
        If you encounter any issues, please double-check the URL or try again later. Not all links may be supported, or there might be temporary issues.

        {0}
        ",
        Command::descriptions()
    };

    match command {
        Command::Help => {
            // Send the comprehensive guide message for /help
            api.send_text_message(message.chat.id, message.id, &comprehensive_guide)
                .await?;
        }
        Command::Start => {
            // Send the comprehensive guide message for /start
            api.send_text_message(message.chat.id, message.id, &comprehensive_guide)
                .await?;
        }
        Command::Download(url) => {
            // Call our core logic with the extracted URL
            process_download_request(
                &url,
                message.chat.id,
                message.id,
                downloader.as_ref(),
                api.as_ref(),
            )
            .await;

            // After sending, the real downloader leaves files in /tmp.
            // A robust solution would also clean these up. For now, the OS will.
        }
    }

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
    #[command(description = "display this help message.")]
    Help,
    #[command(
        description = "download videos from a URL. Usage: /download URL",
        parse_with = "split"
    )]
    Download(String),
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let version = env!("CARGO_PACKAGE_VERSION");
    log::info!("Starting CrabberBot version {}", version);

    let bot = Bot::from_env();

    // Instantiate our REAL dependencies
    let downloader: Arc<dyn Downloader + Send + Sync> = Arc::new(YtDlpDownloader);
    let api: Arc<dyn TelegramApi + Send + Sync> = Arc::new(TeloxideApi::new(bot.clone()));

    // For Google Cloud Run, we use webhooks
    let addr = ([0, 0, 0, 0], 8080).into();
    let url = std::env::var("WEBHOOK_URL")
        .expect("WEBHOOK_URL env var not set")
        .parse()
        .unwrap();

    let listener = teloxide::update_listeners::webhooks::axum(
        bot.clone(),
        teloxide::update_listeners::webhooks::Options::new(addr, url),
    )
    .await
    .expect("Failed to set webhook");

    let handler = Update::filter_message().branch(
        dptree::entry()
            .filter_command::<Command>()
            .endpoint(handle_command),
    );

    // The dispatcher will inject the dependencies into our handler
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![downloader, api])
        .enable_ctrlc_handler()
        .build()
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error has occurred in the dispatcher"),
        )
        .await;
}
