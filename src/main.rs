use std::sync::Arc;

use teloxide::dispatching::MessageFilterExt;
use teloxide::prelude::*;

// Use our library crate
use crabberbot::downloader::{Downloader, YtDlpDownloader};
use crabberbot::handler::message_handler;
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};

async fn handle_message(
    bot: Bot,
    downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
    message: Message,
) -> ResponseResult<()> {
    if let Some(text) = message.text() {
        // Acknowledge the request for better UX
        bot.send_chat_action(message.chat.id, teloxide::types::ChatAction::Typing)
            .await?;

        // Call our unit-tested handler
        message_handler(
            text,
            message.chat.id,
            message.id,
            downloader.as_ref(),
            api.as_ref(),
        )
        .await;

        // After sending, the real downloader leaves files in /tmp.
        // A robust solution would also clean these up. For now, the OS will.
    }
    Ok(())
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

    let handler = Update::filter_message().branch(Message::filter_text().endpoint(handle_message));

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
