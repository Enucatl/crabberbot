use std::path::PathBuf;
use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, MessageKind,
};

use crate::concurrency::ConcurrencyLimiter;
use crate::handler::{send_long_text, CallbackContext};
use crate::premium::summarizer::Summarizer;
use crate::premium::transcriber::Transcriber;
use crate::premium::{
    GEMINI_INPUT_COST_PER_MILLION_TOKENS, GEMINI_OUTPUT_COST_PER_MILLION_TOKENS,
    MAX_PREMIUM_FILE_DURATION_SECS,
};
use crate::storage::Storage;
use crate::subscription::{
    SubscriptionTier, PRODUCT_SUB_BASIC, PRODUCT_SUB_PRO, PRODUCT_TOPUP_60, TOPUP_PRICE_STARS,
    TOPUP_SECONDS,
};
use crate::telegram_api::TelegramApi;
use crate::terms;

pub async fn handle_subscribe(
    api: Arc<dyn TelegramApi>,
    message: Message,
    storage: Arc<dyn Storage>,
) -> ResponseResult<()> {
    let user_id = message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(message.chat.id.0);
    let sub = storage.get_subscription(user_id).await;
    let status_line = if sub.tier == SubscriptionTier::Free {
        if sub.topup_seconds_available > 0 {
            format!(
                "You have <b>{:.1} AI Minutes</b> of top-up credits remaining.",
                sub.topup_seconds_available as f64 / 60.0
            )
        } else {
            "You are currently on the <b>Free</b> plan.".to_string()
        }
    } else {
        format!(
            "You are on the <b>{}</b> plan with <b>{:.1} AI Minutes</b> remaining.",
            sub.tier,
            sub.remaining_ai_minutes()
        )
    };

    let text = indoc::formatdoc! { "
{status}

<b>Monthly Plans</b> (AI Video Minutes reset each month):

<b>Basic</b> — 50 ⭐ per month
  • 60 AI Video Minutes (audio extraction, transcription + summarization)

<b>Pro</b> — 150 ⭐ per month
  • 200 AI Video Minutes (transcription + summarization)
  • Unlimited audio extraction (free — does not use your minutes)

<b>Top-Up</b> (valid 1 year from purchase):
  • 60 AI Video Minutes — 50 ⭐ (one-time, no subscription needed)
  • Also unlocks audio extraction while balance remains

AI Video Minutes are counted from video duration, not processing time.
Use /terms to read the full Terms of Service before purchasing.
",
        status = status_line
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Basic — 50 ⭐ per month", "sub:basic"),
            InlineKeyboardButton::callback("Pro — 150 ⭐ per month", "sub:pro"),
        ],
        vec![InlineKeyboardButton::callback(
            "Top-Up 60 min — 50 ⭐",
            "topup:60",
        )],
    ]);

    api.send_text_with_keyboard(message.chat.id, message.id, &text, keyboard)
        .await?;
    Ok(())
}

pub async fn handle_grant(
    api: Arc<dyn TelegramApi>,
    message: Message,
    storage: Arc<dyn Storage>,
    args: String,
    owner_chat_id: i64,
) -> ResponseResult<()> {
    if message.chat.id.0 != owner_chat_id {
        return Ok(()); // silently ignore non-owner
    }

    const USAGE: &str =
        "Usage:\n/grant [user_id] &lt;tier&gt; [days]  (tier: basic, pro, ultra, free)\n/grant [user_id] topup &lt;minutes&gt;";
    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let self_uid = || message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(message.chat.id.0);

    // Handle topup grants separately: [user_id] topup <minutes>
    let topup_grant: Option<(i64, i32)> = match parts.as_slice() {
        ["topup", minutes_str] => {
            let m = minutes_str.parse::<i32>().ok().filter(|&m| m > 0);
            match m {
                Some(mins) => Some((self_uid(), mins)),
                None => {
                    api.send_text_message(message.chat.id, message.id, USAGE).await?;
                    return Ok(());
                }
            }
        }
        [user_id_str, "topup", minutes_str] => {
            let uid = user_id_str.parse::<i64>().ok();
            let m = minutes_str.parse::<i32>().ok().filter(|&m| m > 0);
            match (uid, m) {
                (Some(uid), Some(mins)) => Some((uid, mins)),
                _ => {
                    api.send_text_message(message.chat.id, message.id, USAGE).await?;
                    return Ok(());
                }
            }
        }
        _ => None,
    };

    if let Some((target_user_id, minutes)) = topup_grant {
        let seconds = minutes * 60;
        storage.add_topup_seconds(target_user_id, seconds).await;
        api.send_text_message(
            message.chat.id,
            message.id,
            &format!("Granted {} top-up minutes to user_id {}", minutes, target_user_id),
        )
        .await?;
        return Ok(());
    }

    // Parse: [user_id] <tier> [days]
    // user_id and days are numeric; tier is not — so two-token ambiguity is unambiguous.
    let (target_user_id, tier_str, days) = match parts.as_slice() {
        [tier] => (self_uid(), *tier, 36500i64),
        [a, b] if a.parse::<i64>().is_ok() => {
            // user_id tier
            (a.parse::<i64>().unwrap(), *b, 36500i64)
        }
        [tier, days_str] => {
            // tier days
            let d = match days_str.parse::<i64>() {
                Ok(d) if d > 0 => d,
                _ => {
                    api.send_text_message(message.chat.id, message.id, USAGE).await?;
                    return Ok(());
                }
            };
            (self_uid(), *tier, d)
        }
        [user_id_str, tier, days_str] => {
            let uid = match user_id_str.parse::<i64>() {
                Ok(id) => id,
                Err(_) => {
                    api.send_text_message(message.chat.id, message.id, USAGE).await?;
                    return Ok(());
                }
            };
            let d = match days_str.parse::<i64>() {
                Ok(d) if d > 0 => d,
                _ => {
                    api.send_text_message(message.chat.id, message.id, USAGE).await?;
                    return Ok(());
                }
            };
            (uid, *tier, d)
        }
        _ => {
            api.send_text_message(message.chat.id, message.id, USAGE).await?;
            return Ok(());
        }
    };

    let tier = match tier_str.parse::<SubscriptionTier>() {
        Ok(t) => t,
        Err(_) => {
            api.send_text_message(
                message.chat.id,
                message.id,
                "Unknown tier. Valid: free, basic, pro, ultra",
            )
            .await?;
            return Ok(());
        }
    };

    storage
        .upsert_subscription(target_user_id, tier.clone(), days)
        .await;

    let duration_label = if days >= 36500 {
        "permanently".to_string()
    } else {
        format!("for {} days", days)
    };
    api.send_text_message(
        message.chat.id,
        message.id,
        &format!("Granted {} to user_id {} {}", tier, target_user_id, duration_label),
    )
    .await?;
    Ok(())
}

pub async fn handle_support(
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
    text: String,
    owner_chat_id: i64,
) -> ResponseResult<()> {
    let chat_id = message.chat.id;

    if text.trim().is_empty() {
        api.send_text_message(
            chat_id,
            message.id,
            indoc::indoc! {"
Please describe your issue after the command, for example:
<code>/support My subscription did not activate after payment</code>

Note: <b>Telegram support and BotFather cannot help with purchases made through CrabberBot.</b> \
All support is handled directly by us."},
        )
        .await?;
        return Ok(());
    }

    api.send_text_message(
        chat_id,
        message.id,
        "Your support request has been received. We aim to respond within 24 hours.\n\n\
         <b>Note:</b> Telegram support and BotFather cannot assist with purchases made through \
         CrabberBot — all support is handled directly by us.",
    )
    .await?;

    if owner_chat_id != 0 {
        let username = message
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .map(|u| format!("@{u}"))
            .unwrap_or_else(|| "(no username)".to_string());
        let from_user_id = message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(chat_id.0);

        // Always include subscription status and recent charges
        let sub = storage.get_subscription(from_user_id).await;
        let sub_line = format!(
            "Subscription: <b>{}</b> | AI Minutes remaining: <b>{:.1}</b> | Top-up: <b>{} sec</b>",
            sub.tier,
            sub.remaining_ai_minutes(),
            sub.topup_seconds_available,
        );
        let payments = storage.get_recent_payments(from_user_id, 5).await;
        let charge_lines = if payments.is_empty() {
            "No charges on record.".to_string()
        } else {
            let mut s = String::new();
            for p in &payments {
                let date = p.created_at.format("%Y-%m-%d %H:%M UTC");
                s.push_str(&format!(
                    "\n<code>/refund {from_user_id} {} {}</code>  {}⭐ ({date})",
                    p.telegram_charge_id, p.product, p.amount,
                ));
            }
            s.trim_start_matches('\n').to_string()
        };

        let relay = format!(
            "[Support] from {username} (user_id: <code>{from_user_id}</code>, chat_id: <code>{chat_id}</code>)\n\
             {sub_line}\n\
             {charge_lines}\n\n\
             {text}\n\n\
             Reply: <code>/reply {chat_id} your message here</code>",
        );
        let _ = api.send_text_no_reply(ChatId(owner_chat_id), &relay).await;
    }

    Ok(())
}

pub async fn handle_reply(
    api: Arc<dyn TelegramApi>,
    message: Message,
    args: String,
    owner_chat_id: i64,
) -> ResponseResult<()> {
    if message.chat.id.0 != owner_chat_id {
        return Ok(());
    }
    let (chat_id_str, reply_text) = match args.trim().split_once(char::is_whitespace) {
        Some(pair) => pair,
        None => {
            api.send_text_message(
                message.chat.id,
                message.id,
                "Usage: /reply &lt;chat_id&gt; &lt;message&gt;",
            )
            .await?;
            return Ok(());
        }
    };
    let target: i64 = match chat_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            api.send_text_message(message.chat.id, message.id, "Invalid chat_id.").await?;
            return Ok(());
        }
    };
    let text = format!("<b>Support reply:</b>\n{}", reply_text.trim());
    let _ = api.send_text_no_reply(ChatId(target), &text).await;
    api.send_text_message(message.chat.id, message.id, "Reply sent.").await?;
    Ok(())
}

pub async fn handle_refundme(
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
) -> ResponseResult<()> {
    let chat_id = message.chat.id;
    let user_id = message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(chat_id.0);

    let payment = match storage.get_latest_payment(user_id).await {
        Some(p) => p,
        None => {
            api.send_text_message(
                chat_id,
                message.id,
                "No purchases found on your account. If you believe this is an error, \
                 please contact /support.",
            )
            .await?;
            return Ok(());
        }
    };

    if storage.has_ai_usage_since(user_id, payment.created_at).await {
        api.send_text_message(
            chat_id,
            message.id,
            "AI features were used after your most recent purchase, so it is considered \
             delivered and is <b>not eligible for an automatic refund</b>.\n\n\
             If you believe this is wrong or experienced a technical failure, \
             please contact /support within 72 hours of your purchase.",
        )
        .await?;
        return Ok(());
    }

    // No AI usage since purchase — auto-refund via Telegram Stars API
    if let Err(e) = api.refund_star_payment(user_id, &payment.telegram_charge_id).await {
        log::warn!("Telegram refund API error for user {}: {}", user_id, e);
        api.send_text_message(
            chat_id,
            message.id,
            "The refund could not be processed automatically. Please contact /support \
             and we will handle it manually.",
        )
        .await?;
        return Ok(());
    }

    // Revoke access
    match payment.product.as_str() {
        PRODUCT_SUB_BASIC | PRODUCT_SUB_PRO => {
            storage.revoke_subscription(user_id).await;
        }
        PRODUCT_TOPUP_60 => {
            storage.revoke_topup(user_id, TOPUP_SECONDS).await;
        }
        _ => {
            log::warn!("Unknown product in /refundme for user {}: {}", user_id, payment.product);
        }
    }

    api.send_text_message(
        chat_id,
        message.id,
        "Your refund has been processed. The Stars have been returned to your Telegram account \
         and your subscription/top-up has been deactivated.",
    )
    .await?;
    Ok(())
}

pub async fn handle_refund(
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
    args: String,
    owner_chat_id: i64,
) -> ResponseResult<()> {
    if message.chat.id.0 != owner_chat_id {
        return Ok(());
    }
    // Usage: /refund <user_id> [<telegram_charge_id> <product>]
    // With just a user_id, shows the 5 most recent charges ready to copy-paste.
    let parts: Vec<&str> = args.trim().splitn(3, char::is_whitespace).collect();

    // /refund <user_id> — list recent charges
    if let [user_id_str] = parts.as_slice() {
        let uid: i64 = match user_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                api.send_text_message(message.chat.id, message.id, "Invalid user_id.").await?;
                return Ok(());
            }
        };
        let payments = storage.get_recent_payments(uid, 5).await;
        if payments.is_empty() {
            api.send_text_message(
                message.chat.id,
                message.id,
                &format!("No payments found for user_id {uid}."),
            )
            .await?;
        } else {
            let mut lines = format!("Recent charges for user_id {uid} — tap to copy:\n");
            for p in &payments {
                let date = p.created_at.format("%Y-%m-%d %H:%M UTC");
                lines.push_str(&format!(
                    "\n<code>/refund {uid} {} {}</code>  — {}⭐ ({date})",
                    p.telegram_charge_id, p.product, p.amount,
                ));
            }
            api.send_text_message(message.chat.id, message.id, &lines).await?;
        }
        return Ok(());
    }

    let (user_id_str, charge_id, product) = match parts.as_slice() {
        [u, ch, p] => (*u, *ch, *p),
        _ => {
            api.send_text_message(
                message.chat.id,
                message.id,
                "Usage: /refund &lt;user_id&gt; [&lt;charge_id&gt; &lt;product&gt;]\n\
                 /refund &lt;user_id&gt; alone shows recent charges.\n\
                 product: sub_basic | sub_pro | topup_60",
            )
            .await?;
            return Ok(());
        }
    };
    let target_user_id: i64 = match user_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            api.send_text_message(message.chat.id, message.id, "Invalid user_id.").await?;
            return Ok(());
        }
    };

    if let Err(e) = api.refund_star_payment(target_user_id, charge_id).await {
        api.send_text_message(
            message.chat.id,
            message.id,
            &format!("Telegram refund API call failed: {e}"),
        )
        .await?;
        return Ok(());
    }

    // Revoke access based on what was refunded
    match product {
        PRODUCT_SUB_BASIC | PRODUCT_SUB_PRO => {
            storage.revoke_subscription(target_user_id).await;
        }
        PRODUCT_TOPUP_60 => {
            storage.revoke_topup(target_user_id, TOPUP_SECONDS).await;
        }
        _ => {
            log::warn!("Unknown product in /refund: {}", product);
        }
    }

    // Notify the user. For private chats user_id == chat_id; for groups we send to user_id directly.
    let _ = api
        .send_text_no_reply(
            ChatId(target_user_id),
            "Your refund has been processed. The Stars have been returned to your account. \
             Any associated subscription or top-up credits have been deactivated.",
        )
        .await;

    api.send_text_message(
        message.chat.id,
        message.id,
        &format!("Refund issued and access revoked for user_id {target_user_id}."),
    )
    .await?;
    Ok(())
}

pub async fn handle_successful_payment(
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
) -> ResponseResult<()> {
    let payment = match message.successful_payment() {
        Some(p) => p,
        None => return Ok(()),
    };

    let chat_id = message.chat.id;
    // Subscription is keyed by user_id so it follows the person across all chats.
    let user_id = message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(chat_id.0);
    let product = &payment.invoice_payload;
    let amount = payment.total_amount;

    storage
        .record_payment(
            user_id,
            &payment.telegram_payment_charge_id.0,
            &payment.provider_payment_charge_id,
            product,
            amount as i32,
        )
        .await;

    match product.as_str() {
        PRODUCT_SUB_BASIC => {
            storage
                .upsert_subscription(user_id, SubscriptionTier::Basic, 30)
                .await;
            api.send_text_message(
                chat_id,
                message.id,
                "Thank you! Your <b>Basic</b> subscription is now active.\n\
                 You have <b>60 AI Video Minutes</b> this month.",
            )
            .await?;
        }
        PRODUCT_SUB_PRO => {
            storage
                .upsert_subscription(user_id, SubscriptionTier::Pro, 30)
                .await;
            api.send_text_message(
                chat_id,
                message.id,
                "Thank you! Your <b>Pro</b> subscription is now active.\n\
                 You have <b>200 AI Video Minutes</b> this month + unlimited audio extraction.",
            )
            .await?;
        }
        PRODUCT_TOPUP_60 => {
            storage.add_topup_seconds(user_id, TOPUP_SECONDS).await;
            api.send_text_message(
                chat_id,
                message.id,
                "Thank you! <b>60 AI Video Minutes</b> have been added to your account. \
                 These are valid for 1 year from today.",
            )
            .await?;
        }
        _ => {
            log::warn!("Unknown payment product: {}", product);
        }
    }

    Ok(())
}

pub async fn handle_refunded_payment(
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    message: Message,
) -> ResponseResult<()> {
    let refund = match &message.kind {
        MessageKind::RefundedPayment(r) => &r.refunded_payment,
        _ => return Ok(()),
    };
    let chat_id = message.chat.id;
    let user_id = message.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(chat_id.0);
    let product = &refund.invoice_payload;
    log::info!(
        "Refunded payment: user_id={} product={} charge_id={}",
        user_id,
        product,
        refund.telegram_payment_charge_id.0
    );
    match product.as_str() {
        PRODUCT_SUB_BASIC | PRODUCT_SUB_PRO => {
            storage.revoke_subscription(user_id).await;
        }
        PRODUCT_TOPUP_60 => {
            storage.revoke_topup(user_id, TOPUP_SECONDS).await;
        }
        _ => {
            log::warn!("Unknown product in refunded_payment: {}", product);
        }
    }
    api.send_text_message(
        chat_id,
        message.id,
        "Your refund has been processed. Any associated subscription or top-up credits \
         have been deactivated.",
    )
    .await?;
    Ok(())
}

pub async fn handle_pre_checkout_query(
    _bot: Bot,
    api: Arc<dyn TelegramApi>,
    query: PreCheckoutQuery,
) -> ResponseResult<()> {
    let payload = &query.invoice_payload;
    let ok = payload.starts_with("sub_") || payload.starts_with("topup_");
    let error_msg: Option<String> = if ok {
        None
    } else {
        Some("Unknown product".to_string())
    };
    api.answer_pre_checkout_query(&query.id.0, ok, error_msg)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// handle_callback_query — thin dispatcher + extracted sub-handlers
// ---------------------------------------------------------------------------

pub async fn handle_callback_query(
    _bot: Bot,
    api: Arc<dyn TelegramApi>,
    storage: Arc<dyn Storage>,
    premium_limiter: Arc<ConcurrencyLimiter>,
    transcriber: Arc<dyn Transcriber>,
    summarizer: Arc<dyn Summarizer>,
    query: CallbackQuery,
) -> ResponseResult<()> {
    let data = match query.data.as_deref() {
        Some(d) => d.to_string(),
        None => return Ok(()),
    };
    let (chat_id, message_id) = match query.message.as_ref() {
        Some(teloxide::types::MaybeInaccessibleMessage::Regular(msg)) => (msg.chat.id, msg.id),
        Some(teloxide::types::MaybeInaccessibleMessage::Inaccessible(msg)) => (msg.chat.id, msg.message_id),
        None => return Ok(()),
    };
    // Subscription is keyed by user_id, not chat_id, so premium features work in group chats.
    let user_id = query.from.id.0 as i64;

    // Always dismiss spinner immediately
    let _ = api.answer_callback_query(&query.id.0, None::<String>).await;

    // Subscription/top-up button presses: show T&C confirmation before sending invoice
    if data == "sub:basic" || data == "sub:pro" || data == "topup:60" {
        return handle_subscription_button(&data, chat_id, message_id, &*api).await;
    }

    // User confirmed T&C and wants to proceed with the invoice
    if let Some(payload) = data.strip_prefix("agree:") {
        return handle_agree_button(payload, chat_id, &*api).await;
    }

    if data == "cancel:purchase" {
        let _ = api.send_text_message(chat_id, message_id, "Purchase cancelled.").await;
        return Ok(());
    }

    // Parse action:context_id
    let (action, context_id_str) = match data.split_once(':') {
        Some(pair) => pair,
        None => return Ok(()),
    };
    let context_id: i32 = match context_id_str.parse() {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    let ctx = match storage.get_callback_context(context_id).await {
        Some(ctx) => ctx,
        None => {
            let _ = api
                .send_text_message(
                    chat_id,
                    message_id,
                    "This action has expired. Please download the video again.",
                )
                .await;
            return Ok(());
        }
    };

    // Check audio cache file exists
    let audio_path = match &ctx.audio_cache_path {
        Some(p) => PathBuf::from(p),
        None => {
            let _ = api
                .send_text_message(
                    chat_id,
                    message_id,
                    "This action has expired. Please download the video again.",
                )
                .await;
            return Ok(());
        }
    };

    if !audio_path.exists() {
        let _ = api
            .send_text_message(
                chat_id,
                message_id,
                "This action has expired. Please download the video again.",
            )
            .await;
        return Ok(());
    }

    // Lock by user_id, not chat_id, so the same person can't double-spend across group chats.
    let _guard = match premium_limiter.try_lock(ChatId(user_id)) {
        Some(g) => g,
        None => {
            let _ = api
                .send_text_message(
                    chat_id,
                    message_id,
                    "I'm already processing a premium action for you. Please wait.",
                )
                .await;
            return Ok(());
        }
    };

    match action {
        "audio" => handle_audio_extraction(&ctx, user_id, chat_id, message_id, &*api, &*storage).await?,
        "txn" => handle_transcription(context_id, &ctx, user_id, chat_id, message_id, &*api, &*storage, &*transcriber, &*summarizer).await?,
        "sum" => handle_summarization(context_id, &ctx, user_id, chat_id, message_id, &*api, &*storage, &*transcriber, &*summarizer).await?,
        _ => {}
    }

    Ok(())
}

async fn handle_subscription_button(
    data: &str,
    chat_id: ChatId,
    message_id: MessageId,
    api: &dyn TelegramApi,
) -> ResponseResult<()> {
    let (product_name, price, agree_data) = match data {
        "sub:basic" => (
            "Basic — 60 AI Video Minutes/month",
            SubscriptionTier::Basic.price_stars(),
            concat!("agree:", "sub_basic"),
        ),
        "sub:pro" => (
            "Pro — 200 AI Video Minutes/month + unlimited audio extraction",
            SubscriptionTier::Pro.price_stars(),
            concat!("agree:", "sub_pro"),
        ),
        _ => (
            "Top-Up — 60 AI Video Minutes (valid 1 year)",
            TOPUP_PRICE_STARS,
            concat!("agree:", "topup_60"),
        ),
    };
    let prompt = terms::terms_pre_purchase_prompt(product_name, price);
    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(
            format!("I Agree & Buy — {} ⭐", price),
            agree_data,
        ),
        InlineKeyboardButton::callback("Cancel", "cancel:purchase"),
    ]]);
    let _ = api.send_text_with_keyboard(chat_id, message_id, &prompt, keyboard).await;
    Ok(())
}

async fn handle_agree_button(
    payload: &str,
    chat_id: ChatId,
    api: &dyn TelegramApi,
) -> ResponseResult<()> {
    let (title, description, amount) = match payload {
        PRODUCT_SUB_BASIC => (
            "Basic Subscription",
            "60 AI Video Minutes/month (counted from video duration)",
            SubscriptionTier::Basic.price_stars(),
        ),
        PRODUCT_SUB_PRO => (
            "Pro Subscription",
            "200 AI Video Minutes/month + unlimited audio extraction",
            SubscriptionTier::Pro.price_stars(),
        ),
        _ => (
            "Top-Up 60 AI Video Minutes",
            "60 AI Video Minutes valid for 1 year from purchase",
            TOPUP_PRICE_STARS,
        ),
    };
    let _ = api.send_invoice(chat_id, title, description, payload, amount).await;
    Ok(())
}

async fn handle_audio_extraction(
    ctx: &CallbackContext,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    api: &dyn TelegramApi,
    storage: &dyn Storage,
) -> ResponseResult<()> {
    let sub = storage.get_subscription(user_id).await;
    let duration_secs = ctx.media_duration_secs.unwrap_or(0);
    if !sub.can_extract_audio(duration_secs) {
        let msg = if sub.total_available_seconds() == 0 {
            "Audio extraction requires a subscription or top-up credits. Use /subscribe to get started.".to_string()
        } else {
            format!(
                "You have {:.1} AI Video Minutes remaining, but this video is {:.1} minutes long. \
                 Need more? /subscribe to upgrade or buy a top-up.",
                sub.remaining_ai_minutes(),
                duration_secs as f64 / 60.0,
            )
        };
        let _ = api.send_text_message(chat_id, message_id, &msg).await;
        return Ok(());
    }

    let audio_path = PathBuf::from(ctx.audio_cache_path.as_deref().unwrap_or(""));
    if let Err(e) = api.send_audio(chat_id, message_id, &audio_path, "Extracted audio").await {
        log::error!("Failed to send audio: {}", e);
        let _ = api
            .send_text_message(chat_id, message_id, "Sorry, failed to send the audio.")
            .await;
        return Ok(());
    }
    // Pro gets unlimited free extraction; everyone else consumes their AI Video Minutes.
    if sub.tier != SubscriptionTier::Pro {
        storage.consume_ai_seconds(user_id, duration_secs).await;
    }
    storage
        .record_premium_usage(user_id, "audio_extract", &ctx.source_url, duration_secs, 0.0, 0.0)
        .await;
    Ok(())
}

async fn handle_transcription(
    context_id: i32,
    ctx: &CallbackContext,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    api: &dyn TelegramApi,
    storage: &dyn Storage,
    transcriber: &dyn Transcriber,
    summarizer: &dyn Summarizer,
) -> ResponseResult<()> {
    let sub = storage.get_subscription(user_id).await;
    let duration_secs = ctx.media_duration_secs.unwrap_or(0);

    if duration_secs > MAX_PREMIUM_FILE_DURATION_SECS {
        let _ = api
            .send_text_message(
                chat_id,
                message_id,
                &format!(
                    "AI features are limited to videos under {} minutes.",
                    MAX_PREMIUM_FILE_DURATION_SECS / 60
                ),
            )
            .await;
        return Ok(());
    }

    if !sub.can_use_ai(duration_secs) {
        let _ = api
            .send_text_message(
                chat_id,
                message_id,
                &format!(
                    "You have {:.1} AI Minutes remaining. Need more? /subscribe to upgrade or buy a top-up.",
                    sub.remaining_ai_minutes()
                ),
            )
            .await;
        return Ok(());
    }

    api.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await?;

    // Use cached transcript if available; otherwise call Deepgram and persist.
    // deepgram_usage = Some((billed_duration_secs, cost_usd)) when Deepgram was called.
    let (raw_transcript, detected_language, deepgram_usage) =
        if let Some(cached) = &ctx.transcript {
            (cached.clone(), ctx.transcript_language.clone(), None)
        } else {
            let audio_path = PathBuf::from(ctx.audio_cache_path.as_deref().unwrap_or(""));
            match transcriber.transcribe(&audio_path).await {
                Ok(t) => {
                    storage
                        .cache_transcript(context_id, &t.transcript, t.detected_language.as_deref())
                        .await;
                    (t.transcript, t.detected_language, Some((t.billed_duration_secs, t.cost_usd)))
                }
                Err(e) => {
                    log::error!("Transcription failed: {}", e);
                    let _ = api
                        .send_text_message(
                            chat_id,
                            message_id,
                            "Sorry, transcription failed. Please try again later.",
                        )
                        .await;
                    return Ok(()); // no quota deduction
                }
            }
        };

    let (transcript, correction_prompt_tokens, correction_output_tokens) = match summarizer
        .correct_transcript(&raw_transcript, detected_language)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            log::error!("Transcript correction failed: {}", e);
            (raw_transcript, 0, 0)
        }
    };

    send_long_text(chat_id, message_id, &transcript, api).await;

    // Only deduct quota and record Deepgram cost when it was actually called.
    if let Some((billed_secs, cost)) = deepgram_usage {
        storage.consume_ai_seconds(user_id, duration_secs).await;
        storage
            .record_premium_usage(user_id, "transcribe", &ctx.source_url, duration_secs, billed_secs, cost)
            .await;
    }
    // Always record Gemini token usage for analytics (does not affect user quota).
    if correction_prompt_tokens > 0 {
        let input_cost = correction_prompt_tokens as f64 / 1_000_000.0 * GEMINI_INPUT_COST_PER_MILLION_TOKENS;
        storage
            .record_premium_usage(user_id, "gemini_correction_input", &ctx.source_url, 0, correction_prompt_tokens as f64, input_cost)
            .await;
    }
    if correction_output_tokens > 0 {
        let output_cost = correction_output_tokens as f64 / 1_000_000.0 * GEMINI_OUTPUT_COST_PER_MILLION_TOKENS;
        storage
            .record_premium_usage(user_id, "gemini_correction_output", &ctx.source_url, 0, correction_output_tokens as f64, output_cost)
            .await;
    }
    Ok(())
}

async fn handle_summarization(
    context_id: i32,
    ctx: &CallbackContext,
    user_id: i64,
    chat_id: ChatId,
    message_id: MessageId,
    api: &dyn TelegramApi,
    storage: &dyn Storage,
    transcriber: &dyn Transcriber,
    summarizer: &dyn Summarizer,
) -> ResponseResult<()> {
    let sub = storage.get_subscription(user_id).await;
    let duration_secs = ctx.media_duration_secs.unwrap_or(0);

    if duration_secs > MAX_PREMIUM_FILE_DURATION_SECS {
        let _ = api
            .send_text_message(
                chat_id,
                message_id,
                &format!(
                    "AI features are limited to videos under {} minutes.",
                    MAX_PREMIUM_FILE_DURATION_SECS / 60
                ),
            )
            .await;
        return Ok(());
    }

    if !sub.can_use_ai(duration_secs) {
        let _ = api
            .send_text_message(
                chat_id,
                message_id,
                &format!(
                    "You have {:.1} AI Minutes remaining. Need more? /subscribe to upgrade or buy a top-up.",
                    sub.remaining_ai_minutes()
                ),
            )
            .await;
        return Ok(());
    }

    api.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await?;

    // Use cached transcript if available; otherwise call Deepgram and persist.
    // deepgram_usage = Some((billed_duration_secs, cost_usd)) when Deepgram was called.
    let (raw_transcript, detected_language, deepgram_usage) =
        if let Some(cached) = &ctx.transcript {
            (cached.clone(), ctx.transcript_language.clone(), None)
        } else {
            let audio_path = PathBuf::from(ctx.audio_cache_path.as_deref().unwrap_or(""));
            match transcriber.transcribe(&audio_path).await {
                Ok(t) => {
                    storage
                        .cache_transcript(context_id, &t.transcript, t.detected_language.as_deref())
                        .await;
                    (t.transcript, t.detected_language, Some((t.billed_duration_secs, t.cost_usd)))
                }
                Err(e) => {
                    log::error!("Transcription failed: {}", e);
                    let _ = api
                        .send_text_message(
                            chat_id,
                            message_id,
                            "Sorry, transcription failed. Please try again later.",
                        )
                        .await;
                    return Ok(()); // no quota deduction
                }
            }
        };

    let (summary, summarize_prompt_tokens, summarize_output_tokens) = match summarizer.summarize(&raw_transcript, detected_language).await {
        Ok(result) => result,
        Err(e) => {
            log::error!("Summarization failed: {}", e);
            let _ = api
                .send_text_message(
                    chat_id,
                    message_id,
                    "Sorry, summarization failed. Please try again later.",
                )
                .await;
            return Ok(()); // no quota deduction
        }
    };

    send_long_text(chat_id, message_id, &summary, api).await;

    // Only deduct quota and record Deepgram cost when it was actually called.
    if let Some((billed_secs, cost)) = deepgram_usage {
        storage.consume_ai_seconds(user_id, duration_secs).await;
        storage
            .record_premium_usage(user_id, "summarize", &ctx.source_url, duration_secs, billed_secs, cost)
            .await;
    }
    // Always record Gemini token usage for analytics (does not affect user quota).
    if summarize_prompt_tokens > 0 {
        let input_cost = summarize_prompt_tokens as f64 / 1_000_000.0 * GEMINI_INPUT_COST_PER_MILLION_TOKENS;
        storage
            .record_premium_usage(user_id, "gemini_summarize_input", &ctx.source_url, 0, summarize_prompt_tokens as f64, input_cost)
            .await;
    }
    if summarize_output_tokens > 0 {
        let output_cost = summarize_output_tokens as f64 / 1_000_000.0 * GEMINI_OUTPUT_COST_PER_MILLION_TOKENS;
        storage
            .record_premium_usage(user_id, "gemini_summarize_output", &ctx.source_url, 0, summarize_output_tokens as f64, output_cost)
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::premium::summarizer::MockSummarizer;
    use crate::premium::transcriber::{MockTranscriber, TranscriptionResult};
    use crate::storage::MockStorage;
    use crate::subscription::{SubscriptionInfo, SubscriptionTier};
    use crate::telegram_api::MockTelegramApi;
    use teloxide::types::{ChatId, MessageId};

    // ---------------------------------------------------------------------------
    // Test helpers
    // ---------------------------------------------------------------------------

    fn make_message(json: serde_json::Value) -> Message {
        serde_json::from_value(json).expect("valid message JSON")
    }

    fn base_message_json(chat_id: i64, user_id: u64) -> serde_json::Value {
        serde_json::json!({
            "message_id": 1,
            "date": 0,
            "chat": {"id": chat_id, "type": "private"},
            "from": {"id": user_id, "is_bot": false, "first_name": "Test"}
        })
    }

    fn active_pro_sub() -> SubscriptionInfo {
        SubscriptionInfo {
            tier: SubscriptionTier::Pro,
            ai_seconds_used: 12000,
            ai_seconds_limit: 12000,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(30)),
        }
    }

    // ---------------------------------------------------------------------------
    // handle_successful_payment
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_successful_payment_basic_subscription() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        mock_storage
            .expect_record_payment()
            .times(1)
            .returning(|_, _, _, _, _| ());
        mock_storage
            .expect_upsert_subscription()
            .withf(|_, tier, days| *tier == SubscriptionTier::Basic && *days == 30)
            .times(1)
            .returning(|_, _, _| ());
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        let mut msg_json = base_message_json(100, 200);
        msg_json["successful_payment"] = serde_json::json!({
            "currency": "XTR",
            "total_amount": 50,
            "invoice_payload": "sub_basic",
            "telegram_payment_charge_id": "tg_charge_123",
            "provider_payment_charge_id": "prov_charge_123"
        });
        let message = make_message(msg_json);

        handle_successful_payment(Arc::new(mock_api), Arc::new(mock_storage), message)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_handle_successful_payment_topup() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        mock_storage
            .expect_record_payment()
            .times(1)
            .returning(|_, _, _, _, _| ());
        mock_storage
            .expect_add_topup_seconds()
            .withf(|_, seconds| *seconds == TOPUP_SECONDS)
            .times(1)
            .returning(|_, _| ());
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        let mut msg_json = base_message_json(100, 200);
        msg_json["successful_payment"] = serde_json::json!({
            "currency": "XTR",
            "total_amount": 50,
            "invoice_payload": "topup_60",
            "telegram_payment_charge_id": "tg_charge_456",
            "provider_payment_charge_id": "prov_charge_456"
        });
        let message = make_message(msg_json);

        handle_successful_payment(Arc::new(mock_api), Arc::new(mock_storage), message)
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_refunded_payment
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_refunded_payment_revokes_subscription() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        mock_storage
            .expect_revoke_subscription()
            .times(1)
            .returning(|_| ());
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        let mut msg_json = base_message_json(100, 200);
        msg_json["refunded_payment"] = serde_json::json!({
            "currency": "XTR",
            "total_amount": 50,
            "invoice_payload": "sub_basic",
            "telegram_payment_charge_id": "tg_charge_123"
        });
        let message = make_message(msg_json);

        handle_refunded_payment(Arc::new(mock_api), Arc::new(mock_storage), message)
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_pre_checkout_query
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_pre_checkout_query_valid_payload() {
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_answer_pre_checkout_query()
            .withf(|_, ok, err| *ok && err.is_none())
            .times(1)
            .returning(|_, _, _| Ok(()));

        let query: PreCheckoutQuery = serde_json::from_value(serde_json::json!({
            "id": "pq_123",
            "from": {"id": 200, "is_bot": false, "first_name": "Test"},
            "currency": "XTR",
            "total_amount": 50,
            "invoice_payload": "sub_basic"
        }))
        .unwrap();

        handle_pre_checkout_query(
            teloxide::Bot::new("fake_token"),
            Arc::new(mock_api),
            query,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_handle_pre_checkout_query_invalid_payload() {
        let mut mock_api = MockTelegramApi::new();
        mock_api
            .expect_answer_pre_checkout_query()
            .withf(|_, ok, err| !ok && err.is_some())
            .times(1)
            .returning(|_, _, _| Ok(()));

        let query: PreCheckoutQuery = serde_json::from_value(serde_json::json!({
            "id": "pq_999",
            "from": {"id": 200, "is_bot": false, "first_name": "Test"},
            "currency": "XTR",
            "total_amount": 99,
            "invoice_payload": "unknown_product"
        }))
        .unwrap();

        handle_pre_checkout_query(
            teloxide::Bot::new("fake_token"),
            Arc::new(mock_api),
            query,
        )
        .await
        .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_support
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_support_empty_text_shows_prompt() {
        let mut mock_api = MockTelegramApi::new();
        let mock_storage = MockStorage::new();

        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        let message = make_message(base_message_json(100, 200));

        handle_support(
            Arc::new(mock_api),
            Arc::new(mock_storage),
            message,
            "".to_string(),
            0,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_handle_support_relays_to_owner() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        // Sends ack to user
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));
        // Relays to owner
        mock_api
            .expect_send_text_no_reply()
            .withf(|chat_id, _| chat_id.0 == 999)
            .times(1)
            .returning(|_, _| Ok(()));
        mock_storage
            .expect_get_subscription()
            .times(1)
            .returning(|_| crate::subscription::SubscriptionInfo::free_default());
        mock_storage
            .expect_get_recent_payments()
            .times(1)
            .returning(|_, _| vec![]);

        let message = make_message(base_message_json(100, 200));

        handle_support(
            Arc::new(mock_api),
            Arc::new(mock_storage),
            message,
            "Please help me".to_string(),
            999, // owner_chat_id
        )
        .await
        .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_refund
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_refund_non_owner_silently_ignored() {
        let mock_api = MockTelegramApi::new();
        let mock_storage = MockStorage::new();

        // Non-owner: no calls expected
        let message = make_message(base_message_json(100, 200)); // chat_id=100

        handle_refund(
            Arc::new(mock_api),
            Arc::new(mock_storage),
            message,
            "200 charge_id sub_basic".to_string(),
            999, // owner_chat_id is 999, message is from chat 100
        )
        .await
        .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_audio_extraction
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_handle_callback_audio_insufficient_quota() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        mock_storage
            .expect_get_subscription()
            .returning(|_| SubscriptionInfo::free_default());
        mock_api
            .expect_send_text_message()
            .times(1)
            .returning(|_, _, _| Ok(()));

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(300), // 5 minutes, no quota
            audio_cache_path: Some("/tmp/fake_audio.mp3".to_string()),
            transcript: None,
            transcript_language: None,
        };

        handle_audio_extraction(&ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_handle_callback_audio_pro_unlimited() {
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();

        // Pro subscriber with exhausted monthly minutes
        mock_storage
            .expect_get_subscription()
            .returning(|_| active_pro_sub());
        // Pro does NOT call consume_ai_seconds
        mock_storage
            .expect_record_premium_usage()
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_api
            .expect_send_audio()
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        // Create a real temp file so audio_path.exists() is true in the parent,
        // but handle_audio_extraction itself receives the path via ctx.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600), // 10 minutes — over monthly quota
            audio_cache_path: Some(path),
            transcript: None,
            transcript_language: None,
        };

        handle_audio_extraction(&ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage)
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // Shared helpers for transcription / summarization tests
    // ---------------------------------------------------------------------------

    fn active_basic_with_quota() -> SubscriptionInfo {
        SubscriptionInfo {
            tier: SubscriptionTier::Basic,
            ai_seconds_used: 0,
            ai_seconds_limit: 7200, // 2 hours — well above the 600s test video
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(30)),
        }
    }

    fn make_transcription_result(transcript: &str) -> TranscriptionResult {
        TranscriptionResult {
            transcript: transcript.to_string(),
            detected_language: Some("en".to_string()),
            billed_duration_secs: 60.0,
            cost_usd: 60.0 * crate::premium::DEEPGRAM_COST_PER_SECOND,
        }
    }

    // ---------------------------------------------------------------------------
    // handle_transcription
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_transcription_fresh_calls_deepgram_and_caches() {
        // No cached transcript → Deepgram called, transcript written to DB,
        // quota deducted, and three usage rows recorded.
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let mut mock_transcriber = MockTranscriber::new();
        let mut mock_summarizer = MockSummarizer::new();

        mock_storage.expect_get_subscription().returning(|_| active_basic_with_quota());
        mock_api.expect_send_chat_action().returning(|_, _| Ok(()));

        mock_transcriber
            .expect_transcribe()
            .times(1)
            .returning(|_| Ok(make_transcription_result("raw transcript")));

        mock_storage.expect_cache_transcript().times(1).returning(|_, _, _| ());

        mock_summarizer
            .expect_correct_transcript()
            .times(1)
            .returning(|_, _| Ok(("Corrected transcript.".to_string(), 1000u64, 500u64)));

        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        mock_storage.expect_consume_ai_seconds().times(1).returning(|_, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "transcribe")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_correction_input")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_correction_output")
            .times(1)
            .returning(|_, _, _, _, _, _| ());

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: None,
            transcript_language: None,
        };

        handle_transcription(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_transcription_cached_skips_deepgram_no_quota() {
        // Cached transcript → Deepgram NOT called, quota NOT deducted,
        // only Gemini correction rows recorded.
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let mock_transcriber = MockTranscriber::new(); // no expectations — panics if called
        let mut mock_summarizer = MockSummarizer::new();

        mock_storage.expect_get_subscription().returning(|_| active_basic_with_quota());
        mock_api.expect_send_chat_action().returning(|_, _| Ok(()));

        // cache_transcript must NOT be called since transcript already exists
        mock_summarizer
            .expect_correct_transcript()
            .times(1)
            .returning(|_, _| Ok(("Corrected.".to_string(), 800u64, 400u64)));

        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        // consume_ai_seconds must NOT be called — no expectations set, panics if invoked
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_correction_input")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_correction_output")
            .times(1)
            .returning(|_, _, _, _, _, _| ());

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: Some("cached transcript".to_string()),
            transcript_language: Some("en".to_string()),
        };

        handle_transcription(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_transcription_insufficient_quota() {
        // User has no AI seconds → error message sent, nothing else called.
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let mock_transcriber = MockTranscriber::new();
        let mock_summarizer = MockSummarizer::new();

        mock_storage
            .expect_get_subscription()
            .returning(|_| SubscriptionInfo::free_default()); // 0 seconds
        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: None,
            transcript_language: None,
        };

        handle_transcription(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_transcription_over_duration_limit() {
        // Video exceeds 30-minute cap → error message, nothing else called.
        let mut mock_api = MockTelegramApi::new();
        let mock_storage = MockStorage::new();
        let mock_transcriber = MockTranscriber::new();
        let mock_summarizer = MockSummarizer::new();

        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(MAX_PREMIUM_FILE_DURATION_SECS + 1),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: None,
            transcript_language: None,
        };

        handle_transcription(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // handle_summarization
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_summarization_fresh_calls_deepgram_and_caches() {
        // No cached transcript → Deepgram called, quota deducted,
        // three usage rows recorded (summarize + two Gemini rows).
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let mut mock_transcriber = MockTranscriber::new();
        let mut mock_summarizer = MockSummarizer::new();

        mock_storage.expect_get_subscription().returning(|_| active_basic_with_quota());
        mock_api.expect_send_chat_action().returning(|_, _| Ok(()));

        mock_transcriber
            .expect_transcribe()
            .times(1)
            .returning(|_| Ok(make_transcription_result("raw transcript")));

        mock_storage.expect_cache_transcript().times(1).returning(|_, _, _| ());

        mock_summarizer
            .expect_summarize()
            .times(1)
            .returning(|_, _| Ok(("• Point one\n\n• Point two".to_string(), 1200u64, 60u64)));

        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        mock_storage.expect_consume_ai_seconds().times(1).returning(|_, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "summarize")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_summarize_input")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_summarize_output")
            .times(1)
            .returning(|_, _, _, _, _, _| ());

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: None,
            transcript_language: None,
        };

        handle_summarization(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_summarization_cached_skips_deepgram_no_quota() {
        // Cached transcript → Deepgram NOT called, quota NOT deducted,
        // only Gemini summarize rows recorded.
        let mut mock_api = MockTelegramApi::new();
        let mut mock_storage = MockStorage::new();
        let mock_transcriber = MockTranscriber::new(); // no expectations — panics if called
        let mut mock_summarizer = MockSummarizer::new();

        mock_storage.expect_get_subscription().returning(|_| active_basic_with_quota());
        mock_api.expect_send_chat_action().returning(|_, _| Ok(()));

        mock_summarizer
            .expect_summarize()
            .times(1)
            .returning(|_, _| Ok(("• Point one".to_string(), 900u64, 30u64)));

        mock_api.expect_send_text_message().times(1).returning(|_, _, _| Ok(()));

        // consume_ai_seconds must NOT be called
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_summarize_input")
            .times(1)
            .returning(|_, _, _, _, _, _| ());
        mock_storage
            .expect_record_premium_usage()
            .withf(|_, feature, _, _, _, _| feature == "gemini_summarize_output")
            .times(1)
            .returning(|_, _, _, _, _, _| ());

        let ctx = CallbackContext {
            source_url: "https://example.com/video".to_string(),
            chat_id: 100,
            has_video: true,
            media_duration_secs: Some(600),
            audio_cache_path: Some("/tmp/audio.mp3".to_string()),
            transcript: Some("cached transcript".to_string()),
            transcript_language: Some("it".to_string()),
        };

        handle_summarization(42, &ctx, 200, ChatId(100), MessageId(1), &mock_api, &mock_storage, &mock_transcriber, &mock_summarizer)
            .await
            .unwrap();
    }
}
