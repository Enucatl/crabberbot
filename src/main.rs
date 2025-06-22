use teloxide::prelude::*;

// Use our library crate
use crabberbot::downloader::{Downloader, YtDlpDownloader};
use crabberbot::handler::message_handler;
use crabberbot::telegram_api::{TelegramApi, TeloxideApi};

use std::sync::Arc;

async fn handle_message(
    bot: Bot,
    message: Message,
    downloader: Arc<dyn Downloader + Send + Sync>,
    api: Arc<dyn TelegramApi + Send + Sync>,
) -> ResponseResult<()> {
    if let Some(text) = message.text() {
        // Acknowledge the request for better UX
        bot.send_chat_action(message.chat.id, teloxide::types::ChatAction::Typing).await?;

        // Call our unit-tested handler
        message_handler(text, message.chat.id, message.id, downloader.as_ref(), api.as_ref()).await;

        // After sending, the real downloader leaves files in /tmp.
        // A robust solution would also clean these up. For now, the OS will.
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting CrabberBot...");

    let bot = Bot::from_env();

    // Instantiate our REAL dependencies
    let downloader: Arc<dyn Downloader + Send + Sync> = Arc::new(YtDlpDownloader);
    let api: Arc<dyn TelegramApi + Send + Sync> = Arc::new(TeloxideApi::new(bot.clone()));

    // For Google Cloud Run, we use webhooks
    let addr = ([0, 0, 0, 0], 8080).into();
    let url = std::env::var("WEBHOOK_URL").expect("WEBHOOK_URL env var not set").parse().unwrap();
    
    let listener = teloxide::dispatching::webhooks::axum(bot.clone(), teloxide::dispatching::UpdateHandler::new(dptree::entry()))
        .await;

    log::info!("Setting webhook to: {}", url);
    bot.set_webhook(url).await.expect("Failed to set webhook");

    let handler = Update::filter_message()
        .branch(dptree::endpoint(handle_message));

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
