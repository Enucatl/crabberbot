/// Single source of truth for all Terms of Service text and policy constants.
///
/// Every place that displays terms to users — the /terms command, the pre-purchase
/// confirmation, and the /subscribe screen — must pull from this module. This guarantees
/// the text shown during a purchase is always identical to what /terms displays.

/// How many days top-up credits remain valid after the most recent top-up purchase.
/// Each new top-up purchase resets this window for the entire top-up balance.
pub const TOPUP_EXPIRY_DAYS: i64 = 365;

/// Full Terms of Service displayed by /terms and embedded in pre-purchase confirmations.
pub fn terms_text() -> String {
    format!(
        indoc::indoc! {"
<b>CrabberBot — Terms of Service</b>

<b>What you get</b>
• <b>Basic — 50 ⭐/month:</b> 60 AI Video Minutes (audio extraction, transcription + summarization all draw from this quota)
• <b>Pro — 150 ⭐/month:</b> 200 AI Video Minutes for transcription + summarization, plus unlimited audio extraction that never consumes your minutes
• <b>Top-Up — 50 ⭐ (one-time):</b> 60 AI Video Minutes, no subscription required

<b>What are AI Video Minutes?</b>
AI Video Minutes are consumed based on the <i>duration of the video</i>, not how long processing takes. A 10-minute video costs 10 AI Video Minutes for transcription or summarization, regardless of whether processing completes in 30 seconds or 3 minutes. Audio extraction does not consume AI Video Minutes.

<b>Monthly subscriptions</b>
Subscriptions last 30 days from purchase. AI Video Minutes reset at the start of each billing cycle. Unused monthly minutes do not carry over to the next cycle.

<b>Top-Up credits</b>
Top-Up AI Video Minutes expire {expiry_days} days (1 year) after your most recent top-up purchase. Each new top-up purchase resets this {expiry_days}-day window for your entire top-up balance. Unused credits after expiry are forfeited with no compensation.

<b>Payments</b>
All purchases are made in Telegram Stars (⭐). 1 Star ≈ $0.02 USD. The exact price in your local currency is shown by Telegram before you confirm. Payment is processed by Telegram.

<b>Refunds</b>
If AI features (transcription or summarization) have already been used after a purchase, the service is considered delivered and the purchase is <b>non-refundable</b>. If your purchase was not delivered (subscription did not activate, features were broken), contact /paysupport within 72 hours and we will investigate. Confirmed delivery failures will be refunded in full.

<b>Support</b>
For help, use /support or /paysupport. <b>Telegram support and BotFather cannot assist with purchases made through CrabberBot</b> — all purchase support is handled directly by us.

By completing a purchase, you confirm that you have read and agree to these Terms of Service."},
        expiry_days = TOPUP_EXPIRY_DAYS,
    )
}

/// Short prompt shown to the user above the agree/cancel buttons before an invoice is sent.
/// References /terms so users can read the full text before agreeing.
pub fn terms_pre_purchase_prompt(product_name: &str, price_stars: u32) -> String {
    format!(
        indoc::indoc! {"
You are about to purchase: <b>{product_name} — {price_stars} ⭐</b>

Before completing your purchase, please confirm that you have read and agree to the <b>CrabberBot Terms of Service</b>. Key points:
• AI Video Minutes are counted from <i>video duration</i>, not processing time
• Basic: audio extraction uses your AI Video Minutes quota; Pro: audio extraction is unlimited and free
• Top-Up credits expire after 1 year from last purchase
• No refund once AI features have been used

Use /terms to read the full Terms of Service.

Tap <b>I Agree &amp; Buy</b> to confirm your purchase and accept the terms."},
        product_name = product_name,
        price_stars = price_stars,
    )
}
