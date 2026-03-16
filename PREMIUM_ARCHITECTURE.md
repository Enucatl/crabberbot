# CrabberBot Premium Features Architecture

This document describes the architecture, database design, and business decisions behind CrabberBot's premium features layer. It is intended as a reference for implementation and future maintenance.

## Overview

CrabberBot is a Telegram bot that downloads videos and photos from social media URLs. The premium layer adds three paid features on top of the existing free download functionality:

1. **Extract Audio** - extract the audio track from a downloaded video as an MP3
2. **Transcribe** - speech-to-text transcription of video/audio using Deepgram Nova-3
3. **Summarize** - AI-generated summary of a video's content using Google Gemini

Premium actions are triggered via **inline keyboard buttons** that appear after every successful video download. Users pay with **Telegram Stars**, Telegram's built-in digital currency.

---

## User Experience Flow

```
User sends URL
    |
    v
Bot downloads video via yt-dlp
    |
    +---> Telegram upload (send video to user)    } these run
    +---> ffmpeg audio extraction + ffprobe       } concurrently
    |
    v
Bot sends inline keyboard:
  [ Extract Audio ]  [ Transcribe ]  [ Summarize ]
    |
    v
User taps a button
    |
    +---> Free user with no top-up? --> "Subscribe or buy a top-up: /subscribe"
    +---> Cache expired (>2h)?      --> "Action expired. Download again."
    +---> Quota exceeded?           --> "You have X AI Minutes remaining."
    +---> OK:
            Audio:      send cached .mp3 (no API cost, no quota)
            Transcribe: Deepgram API --> send transcript text
            Summarize:  Deepgram API --> Gemini API --> send summary text
            |
            v
          Deduct AI Seconds (only on success)
```

---

## Products and Pricing

### Monthly Subscriptions

| | Basic | Pro |
|---|---|---|
| Price | 50 Stars/month (~$1.00) | 150 Stars/month (~$3.00) |
| Net revenue (after Telegram ~35% cut) | ~$0.65 | ~$1.95 |
| Monthly AI Video Minutes quota | 60 min | 200 min |
| Audio extraction | Uses AI Video Minutes quota | Unlimited, free (does not use minutes) |
| Max API cost (Deepgram @ $0.0078/min) | $0.47 | $1.56 |
| Worst-case margin | 28% | 20% |
| Realistic margin (~60% breakage) | ~60% | ~65% |

Monthly AI Video Minutes reset to zero on each subscription renewal. Top-up balance is unaffected by renewals.

**AI Video Minutes** are counted from the *duration of the video being processed*, not from processing time. A 10-minute video costs 10 AI Video Minutes for transcription or summarization, regardless of how long processing takes. Audio extraction never consumes AI Video Minutes.

### One-Time Top-Ups

| Product | Price | AI Video Minutes | Expiry |
|---|---|---|---|
| Top-Up | 50 Stars (~$1.00) | 60 min | 365 days from last purchase |

Top-ups are available to **all tiers including Free**. A free-tier user who buys a top-up gets audio extraction and 60 AI Video Minutes without committing to a subscription.

**Top-up expiry:** Credits expire 365 days after the most recent top-up purchase. Each new purchase resets this window for the entire balance. The expiry is enforced in both the application layer (`SubscriptionInfo::active_topup_seconds()`) and as a nightly database cleanup (`expire_stale_topups()`). The constant `terms::TOPUP_EXPIRY_DAYS = 365` is the single source of truth — the terms text, the application check, and the SQL cleanup all reference it.

### Cost Structure

| Service | Cost | Billing model |
|---|---|---|
| ffmpeg audio extraction | $0.00 | Free (local CPU) |
| Deepgram Nova-3 transcription | $0.0078/min ($0.00013/sec) | Per audio second, billed duration from `metadata.duration` in API response |
| Google Gemini summarization | $0.25/M input tokens, $1.50/M output tokens | Per token, token counts from `usageMetadata` in API response |

Gemini's cost is negligible compared to Deepgram, so from a **user quota perspective** Gemini is free. AI Seconds are only deducted when Deepgram is actually called. Gemini costs are recorded for analytics only.

The billed Deepgram duration is taken from `json["metadata"]["duration"]` in the API response, not estimated from video duration. This is the authoritative value Deepgram uses for billing.

### Owner Grants

The bot owner (identified by `OWNER_CHAT_ID`) can grant subscriptions manually via `/grant`:
- `/grant pro` - grant Pro to yourself
- `/grant 123456789 basic` - grant Basic to another user

Grants create 100-year subscriptions with no payment record, so they do not inflate revenue metrics in Grafana.

---

## Two-Bucket Billing Model

The billing system uses two separate quotas stored on a single database row:

```
+---------------------------+
|     subscriptions row     |
+---------------------------+
| ai_seconds_used: 1800     |  <-- Monthly bucket (resets on renewal)
| ai_seconds_limit: 3600    |      "Used 30 of 60 minutes"
+---------------------------+
| topup_seconds_available:  |  <-- Top-up bucket (never resets)
|   1200                    |      "20 minutes of top-up remaining"
+---------------------------+
```

### Consumption Order

When a user triggers a premium action (e.g. transcribe a 10-minute video = 600 seconds):

1. **Monthly first.** If `ai_seconds_limit - ai_seconds_used >= 600`, deduct entirely from monthly.
2. **Overflow to top-up.** If only 200 monthly seconds remain, burn those 200, then deduct the remaining 400 from `topup_seconds_available`.
3. **Single SQL statement.** The two-bucket deduction is atomic:

```sql
UPDATE subscriptions SET
    ai_seconds_used = LEAST(ai_seconds_used + $2, ai_seconds_limit),
    topup_seconds_available = topup_seconds_available
        - GREATEST($2 - (ai_seconds_limit - ai_seconds_used), 0)
WHERE chat_id = $1;
```

### Why Integer Seconds (Not Float Minutes)

All quotas are stored as `INTEGER` seconds, not `REAL` minutes. IEEE 754 floating-point math introduces rounding errors: `30.1 + 29.9` might equal `59.9999999998`. With integers, `1806 + 1794 == 3600` exactly. Values are displayed to users as minutes (divide by 60) at the presentation layer only.

### Top-Up Expiry (365 Days)

Top-up credits expire 365 days after the last top-up purchase. Each new purchase resets the window for the whole balance. The rationale: a 60-minute top-up costs at most ~$0.47 in API fees, so there is minimal financial pressure to expire credits. The 365-day policy exists for terms-of-service clarity and to comply with Telegram's requirement that expiry terms be clearly disclosed to users before purchase.

### Why Free Users Can Use Top-Ups

A free-tier user with top-up balance can still use AI features. This avoids the "telecom trap" where buying credits on the 30th and having them vanish on the 31st infuriates users. It also serves as a low-commitment entry point: try premium features once before subscribing.

---

## Database Schema

The premium layer adds four tables to the existing database (which already has `media_cache`, `cached_files`, and `requests`).

### `subscriptions`

One row per user. Tracks tier, monthly quota, top-up balance, and expiry.

```sql
CREATE TABLE subscriptions (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL UNIQUE,
    tier TEXT NOT NULL DEFAULT 'free',
    ai_seconds_used INTEGER NOT NULL DEFAULT 0,
    ai_seconds_limit INTEGER NOT NULL DEFAULT 0,
    topup_seconds_available INTEGER NOT NULL DEFAULT 0,
    last_topup_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- `ai_seconds_used` / `ai_seconds_limit`: monthly quota (Basic=3600, Pro=12000). Reset to 0/limit on renewal.
- `topup_seconds_available`: lifetime balance. Only decremented by usage, never reset.
- `last_topup_at`: timestamp of most recent top-up purchase. Data capture for future expiry logic.
- `expires_at`: subscription expiry. Top-ups survive expiry.

### `payments`

Immutable ledger of all Telegram Stars transactions.

```sql
CREATE TABLE payments (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    telegram_payment_charge_id TEXT NOT NULL UNIQUE,
    provider_payment_charge_id TEXT NOT NULL,
    product TEXT NOT NULL,
    amount INTEGER NOT NULL,
    currency TEXT NOT NULL DEFAULT 'XTR',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- `product`: one of `'sub_basic'`, `'sub_pro'`, `'topup_60'`.
- `amount`: in Stars.
- Owner grants do **not** create payment rows.

### `premium_usage`

Per-action log for analytics and cost monitoring. Not used for limit-checking.

```sql
CREATE TABLE premium_usage (
    id SERIAL PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    feature TEXT NOT NULL,
    source_url TEXT NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0.0,
    units REAL NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- `feature`: one of `'transcribe'`, `'summarize'`, `'audio_extract'`, `'gemini_correction_input'`, `'gemini_correction_output'`, `'gemini_summarize_input'`, `'gemini_summarize_output'`.
- `estimated_cost_usd`: calculated at write time from API-reported quantities.
- `units`: raw quantity as reported by the API — audio seconds (Deepgram `metadata.duration`) for `'transcribe'`/`'summarize'` rows; token count for Gemini rows. Audio extraction records 0.
- Gemini rows (`gemini_*`) record token costs for analytics only; they do not trigger quota deduction.
- `duration_secs`: video duration from ffprobe, used for quota deduction (Deepgram rows only).

### `callback_contexts`

Maps Telegram callback button data (limited to 64 bytes) back to the full context needed for premium actions.

```sql
CREATE TABLE callback_contexts (
    id SERIAL PRIMARY KEY,
    source_url TEXT NOT NULL,
    chat_id BIGINT NOT NULL,
    has_video BOOLEAN NOT NULL DEFAULT TRUE,
    media_duration_secs INTEGER,
    audio_cache_path TEXT,
    transcript TEXT,
    transcript_language TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

- `media_duration_secs`: from ffprobe (ground truth), not yt-dlp metadata.
- `audio_cache_path`: path to cached `.mp3` in `/tmp/audio_cache/`.
- `transcript`: populated after the first Deepgram call; subsequent Transcribe or Summarize button clicks read from here, skipping Deepgram entirely.
- `transcript_language`: BCP-47 language code (`"it"`, `"en"`, …) detected by Deepgram and cached alongside the transcript.
- Rows older than 24 hours are cleaned up by the hourly task.

### Entity Relationship

```
subscriptions (1) ----< payments (many)
      |
      +---------------< premium_usage (many)
      |
      +---------------< callback_contexts (many)

media_cache (1) ----< cached_files (many)     [existing]
requests                                       [existing]
```

---

## Audio Cache Architecture

### Problem

After a user downloads a video, they might click "Transcribe" 30 seconds later. Re-downloading the video via yt-dlp would be slow, unreliable (rate limiting), and the URL might have expired.

### Solution: Eager Extraction

During every video download, immediately extract the audio track with ffmpeg and store it as a small `.mp3` in `/tmp/audio_cache/`. This happens **concurrently with the Telegram upload** using `tokio::join!`, so the user perceives zero added latency.

```
Download completes
        |
        +---> tokio::join! --->  Upload video to Telegram (2-10s)
        |                  --->  ffprobe duration + ffmpeg extraction (1-3s)
        |
        v
Both done. Send inline keyboard.
User clicks button --> cached .mp3 is already waiting.
```

### Cache Lifecycle

| Event | Action |
|---|---|
| Video downloaded | Extract audio to `/tmp/audio_cache/{uuid}.mp3` |
| User clicks premium button | Read `.mp3` from cache path |
| 2 hours after extraction | Hourly cleanup deletes the file |
| User clicks after cleanup | "This action has expired. Please download the video again." |

### CPU Controls

ffmpeg by default uses all available CPU cores. Two controls prevent CPU starvation:

1. **`-threads 1`** on every ffmpeg command. Each process uses exactly 1 core.
2. **Semaphore with 3 permits.** At most 3 concurrent ffmpeg processes. 3 cores for extraction, remaining cores for the Tokio runtime and webhook handler.

### Duration from ffprobe

File duration is obtained from `ffprobe` on the downloaded file, not from yt-dlp metadata. This is the ground truth: yt-dlp relies on website metadata which can be `None` for Twitter/X videos, certain TikToks, and other platforms. ffprobe works on every format and is nearly instant on local files.

---

## Module Architecture

### Current Modules (Unchanged)

```
src/
  lib.rs              - module declarations, re-exports
  main.rs             - bot startup, dispatch tree, command handlers
  handler.rs          - process_download_request() pipeline
  downloader.rs       - Downloader trait + YtDlpDownloader
  telegram_api.rs     - TelegramApi trait + TeloxideApi
  storage.rs          - Storage trait + PostgresStorage
  validator.rs        - URL validation rules
  concurrency.rs      - ConcurrencyLimiter (per-chat locks)
  test_utils.rs       - shared test helpers
```

### New Modules

```
src/
  terms.rs            - TOPUP_EXPIRY_DAYS constant + terms_text() + terms_pre_purchase_prompt()
  subscription.rs     - SubscriptionTier enum, SubscriptionInfo struct (uses terms::TOPUP_EXPIRY_DAYS)
  premium/
    mod.rs            - PremiumError, cost constants, MAX_PREMIUM_FILE_DURATION_SECS
    audio_extractor.rs - AudioExtractor trait + FfmpegAudioExtractor
    transcriber.rs    - Transcriber trait + DeepgramTranscriber
    summarizer.rs     - Summarizer trait + GeminiSummarizer
```

### Trait Design

All new components follow the existing pattern: a trait for the interface, a concrete implementation, and a mockall mock for testing.

| Trait | Implementation | Purpose |
|---|---|---|
| `AudioExtractor` | `FfmpegAudioExtractor` | ffprobe + ffmpeg audio extraction with semaphore |
| `Transcriber` | `DeepgramTranscriber` | Speech-to-text via Deepgram Nova-3 API |
| `Summarizer` | `GeminiSummarizer` | Text summarization via Google Gemini API |

All traits have `Send + Sync` supertraits. All implementations are injected as `Arc<dyn Trait>` via teloxide's `dptree::deps!`.

### Concurrency Model

Two independent `ConcurrencyLimiter` instances:

| Limiter | Scope | Purpose |
|---|---|---|
| `download_limiter` | `handle_url` | Existing. Prevents concurrent downloads for the same chat. |
| `premium_limiter` | `handle_callback_query` | New. Prevents concurrent premium actions for the same chat. |

Premium actions do not block downloads and vice versa.

---

## Payment Flow

### Telegram Stars Integration

Telegram Stars is Telegram's built-in payment system. No external payment provider is needed.

**Pre-purchase T&C confirmation flow** (required by Telegram Stars ToS):

```
User taps [Basic - 50 Stars/mo]
    |
    v
Bot shows terms summary + [ I Agree & Buy — 50 ⭐ ] [ Cancel ]
    |
    v (user taps I Agree & Buy)
Bot calls send_invoice(provider_token="", currency="XTR", amount=50)
    |
    v
Telegram shows native payment dialog
    |
    v
Telegram sends PreCheckoutQuery to bot
    --> Bot validates payload, responds OK within 10 seconds
    |
    v
Payment succeeds
    --> Telegram sends SuccessfulPayment message
    --> Bot records payment, activates/renews subscription
    --> Bot sends confirmation with quota info
```

### Compliance Commands

Per Telegram Stars terms, the bot must implement:

| Command | Handler | Purpose |
|---|---|---|
| `/terms` | `handle_command` → `terms::terms_text()` | Display full Terms of Service |
| `/support <text>` | `handle_support` | General support; relays to owner via `send_text_no_reply` |
| `/paysupport <text>` | `handle_support` | Payment support; same relay + includes subscription status |
| `/reply <chat_id> <msg>` | `handle_reply` (owner-only, hidden) | Owner replies to support request through bot |
| `/refund <chat_id> <charge_id> <product>` | `handle_refund` (owner-only, hidden) | Issues Telegram refund + revokes access |

The `/reply` and `/refund` commands are hidden (no `description` attribute) and silently ignored for non-owners.

### Refund Policy

**No refund once AI features have been used.** This is stated in the Terms of Service displayed before every purchase. Refunds are only processed for delivery failures (subscription didn't activate, features broken).

When a refund is issued:
- `sub_basic`/`sub_pro`: `revoke_subscription()` — tier set to Free, `expires_at` cleared
- `topup_60`: `revoke_topup(chat_id, 3600)` — reduces `topup_seconds_available` by 3600 (clamped to 0)
- Telegram `refund_star_payment` API is called to return Stars to the user

The `handle_refunded_payment` handler fires if Telegram sends a `RefundedPayment` message (e.g. via a dispute), performing the same revocation.

### Dispatch Tree

```rust
dptree::entry()
    .branch(Update::filter_message()
        .branch(successful_payment_filter)   // MUST be before commands
        .branch(refunded_payment_filter)
        .branch(commands)                     // /start, /version, /subscribe, /terms,
                                             //  /support, /paysupport, /grant, /reply, /refund
        .branch(urls)                         // URL download handler
        .branch(unhandled))
    .branch(Update::filter_callback_query()
        .endpoint(handle_callback_query))    // inline buttons + T&C agree/cancel
    .branch(Update::filter_pre_checkout_query()
        .endpoint(handle_pre_checkout_query))
```

---

## Grafana Monitoring

All monitoring data lives in the bot's PostgreSQL database. No external billing API scraping needed.

### Cost Tracking

Every `record_premium_usage` call records cost calculated from API-reported quantities:

| Feature row | Rate | Source of `units` |
|---|---|---|
| `transcribe` / `summarize` | $0.00013/sec | Deepgram `metadata.duration` (billed audio seconds) |
| `gemini_correction_input` | $0.25 / 1M tokens | Gemini `usageMetadata.promptTokenCount` |
| `gemini_correction_output` | $1.50 / 1M tokens | Gemini `usageMetadata.candidatesTokenCount` |
| `gemini_summarize_input` | $0.25 / 1M tokens | Gemini `usageMetadata.promptTokenCount` |
| `gemini_summarize_output` | $1.50 / 1M tokens | Gemini `usageMetadata.candidatesTokenCount` |
| `audio_extract` | $0.00 | ffmpeg (local CPU) |

Deepgram rows also carry `duration_secs` (video duration from ffprobe) for quota deduction purposes. Gemini rows set `duration_secs = 0` — they are analytics-only.

### Revenue Tracking

Revenue is derived from the `payments` table:
- **Gross revenue:** `amount * $0.02` (1 Star ~ $0.02)
- **Net revenue:** `amount * $0.013` (after Telegram's ~35% cut)

### Dashboard Panels

| Panel | Type | What it shows |
|---|---|---|
| Daily API spend | Time series | Total `estimated_cost_usd` from `premium_usage` per day |
| Spend by provider | Stacked time series | Cost breakdown by feature (transcribe vs summarize) |
| Monthly API cost | Stat | Current month's total API spend |
| Monthly gross revenue | Stat | Current month's Stars revenue (gross) |
| Monthly net revenue | Stat | Current month's Stars revenue (net of Telegram cut) |
| Profit margin | Time series | Net revenue vs API cost over time |
| Revenue by type | Stacked time series | Subscription revenue vs top-up revenue |
| Active subscribers | Stat | Count of non-free users with valid expiry |
| Subscribers by tier | Pie chart | Basic vs Pro distribution |
| Outstanding top-up liability | Stat | Total unspent top-up minutes across all users |

### Data Integrity Notes

- Owner-granted subscriptions create no `payments` rows, so they do not inflate revenue.
- Failed premium actions create no `premium_usage` rows, so they do not inflate costs.
- Cost constants are in `src/premium/mod.rs`. Update them if provider pricing changes.

---

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `TELOXIDE_TOKEN` | Yes | Telegram Bot API token (existing) |
| `DATABASE_URL` | Yes | PostgreSQL connection string (existing) |
| `DEEPGRAM_API_KEY` | For transcription | Deepgram Nova-3 API key |
| `GEMINI_API_KEY` | For summarization | Google Gemini API key |
| `OWNER_CHAT_ID` | For `/grant`, `/reply`, `/refund` | Bot owner's Telegram user ID. Also receives support relay messages. |

---

## Cleanup Schedule

The existing hourly cleanup task is expanded:

| Target | TTL | Method |
|---|---|---|
| `media_cache` rows (existing) | 7 days | SQL `DELETE` |
| `/tmp/audio_cache/*.mp3` | 2 hours | Filesystem `modified()` check |
| `callback_contexts` rows | 24 hours | SQL `DELETE` |
| `topup_seconds_available` balances | 365 days from `last_topup_at` | `expire_stale_topups()` SQL `UPDATE` |

---

## Key Design Decisions Summary

| # | Decision | Rationale |
|---|---|---|
| 1 | Two-bucket billing (monthly + lifetime top-up) | Prevents "telecom trap" of credits vanishing at renewal. Zero financial liability from non-expiring top-ups. |
| 2 | Integer seconds, not float minutes | Eliminates IEEE 754 rounding errors in billing arithmetic. |
| 3 | Unlimited audio extraction (Pro only) | ffmpeg is free (no API cost). Exclusive Pro feature; Basic users and top-up holders can also get it via top-up balance. |
| 4 | Concurrent extraction + upload | `tokio::join!` hides ffmpeg latency behind the Telegram upload. Zero perceived delay. |
| 5 | Semaphore (3) + `-threads 1` | Predictable CPU: 1 permit = 1 core. Prevents ffmpeg's default multi-threaded thrashing. |
| 6 | Duration from ffprobe | Ground truth from the file itself. yt-dlp metadata is unreliable (often `None`). |
| 7 | 30-minute duration gate | Prevents webhook timeouts, Deepgram choking, and RAM issues on large files. |
| 8 | Separate download/premium locks | Premium actions never block downloads and vice versa. |
| 9 | Stars refunds revoke subscription | Instant, automatic. Top-ups require manual refund. |
| 10 | Non-fatal audio extraction | ffmpeg failure doesn't break downloads. Premium buttons are just hidden. |
| 11 | Failed actions are free | AI Seconds deducted only after successful API response. |
| 12 | Consolidated cost monitoring | All costs and revenue in PostgreSQL, queryable from Grafana. No external billing scraping. |
| 13 | Free users can use top-ups | No subscription lock-in. Low-commitment entry point to premium features. |
| 14 | Transcript cached in `callback_contexts` | Deepgram is the most expensive API call (~$0.00013/sec). A second click on Transcribe or Summarize reads `transcript` from the DB, skipping Deepgram entirely. No duplicate billing; AI Seconds deducted only when Deepgram is actually called. |
| 15 | Gemini cost is free to users | Gemini token cost is negligible vs Deepgram. Only Deepgram calls consume AI Seconds quota. Gemini usage is still recorded in `premium_usage` (with separate input/output rows) for cost analytics, but does not deduct from any user balance. |
| 16 | Actual API quantities in `units` column | Both Deepgram (`metadata.duration`) and Gemini (`usageMetadata` token counts) report what they actually billed. Storing these raw numbers in `premium_usage.units` gives undisputable audit data, independent of any application-side estimates. |
| 17 | ID3 tags in MP3 files | ffmpeg `-metadata title=` / `-metadata artist=` embed video title and uploader into the extracted MP3. Tags are truncated to 255 Unicode codepoints (`.chars().take(255)`) — this is safe for multi-byte UTF-8 because `.chars()` iterates over codepoints, not bytes. |
