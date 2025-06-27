use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use url::Url;

// Use our library crate
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
            api.send_text_message(message.chat.id, message.id, &version_message).await?;
        }
    }

    Ok(())
}

async fn handle_url(
    bot: Bot,
    downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
    message: Message,
    url: Url,
) -> ResponseResult<()> {
    bot.send_chat_action(message.chat.id, teloxide::types::ChatAction::Typing).await?;
    process_download_request(
        &url,
        message.chat.id,
        message.id,
        downloader.as_ref(),
        api.as_ref(),
    )
    .await;
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
    pretty_env_logger::init();

    let version = env!("CARGO_PACKAGE_VERSION");
    log::info!("Starting CrabberBot version {}", version);

    let bot = Bot::from_env();

    // Instantiate our REAL dependencies
    let downloader: Arc<dyn Downloader + Send + Sync> = Arc::new(YtDlpDownloader);
    let api: Arc<dyn TelegramApi + Send + Sync> = Arc::new(TeloxideApi::new(bot.clone()));

    // For Google Cloud Run, we use webhooks
    let addr = ([0, 0, 0, 0], 8080).into();
    let webhook_url_str = std::env::var("WEBHOOK_URL").expect("WEBHOOK_URL env var not set");
    let url: Url = webhook_url_str.parse().unwrap();

    let listener = teloxide::update_listeners::webhooks::axum(
        bot.clone(),
        teloxide::update_listeners::webhooks::Options::new(addr, url),
    )
    .await
    .expect("Failed to set webhook");

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
        .dependencies(dptree::deps![downloader, api])
        .enable_ctrlc_handler()
        .build()
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error has occurred in the dispatcher"),
        )
        .await;
}
