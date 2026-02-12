# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

CrabberBot is a Rust-based Telegram bot that downloads videos, photos, and galleries from various websites (YouTube, Instagram, TikTok, Twitter/X, etc.) using a custom fork of yt-dlp. It runs as a webhook-based service behind a Cloudflare tunnel with a local Telegram Bot API server for large file support.

## Build & Test Commands

All build commands require `CARGO_PACKAGE_VERSION`. Generate it with:
```bash
export CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./')
```

**Run tests locally (requires Rust toolchain):**
```bash
cargo test --verbose
```

**Run tests in Docker (mirrors CI):**
```bash
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --profile test run --build --rm test-runner
```

**Build and run locally with Docker:**
```bash
docker compose --env-file .env up --build
```

**CI pipeline** (.github/workflows/deploy.yml): pushes to `main` triggers `cargo test`, Docker build+push to GHCR, then Portainer webhook redeploy.

## Architecture

### Data Flow
User sends URL → Telegram webhook (Axum on :8080) → URL parsing & concurrency check → yt-dlp metadata fetch → validation (duration/size/playlist limits) → yt-dlp download → send media via Telegram API → RAII cleanup of temp files.

### Source Modules (src/)
- **main.rs** — Webhook setup, command dispatch (`/start`, `/version`, `/environment`), dependency injection via `dptree::deps!`. Routes messages to command handler, URL handler, or fallback.
- **handler.rs** — Core orchestration: `process_download_request()` runs a 3-step pipeline (pre-download validation → download & prepare → send single item or media group). Contains `FileCleanupGuard` (Drop-based RAII for temp file deletion).
- **downloader.rs** — `Downloader` trait + `YtDlpDownloader` impl. Shells out to yt-dlp for metadata (`--dump-single-json`) and downloads (`--print-json`). `MediaMetadata` struct handles JSON deserialization, caption building, and media type detection.
- **telegram_api.rs** — `TelegramApi` trait + `TeloxideApi` impl wrapping teloxide::Bot. Methods: `send_video`, `send_photo`, `send_media_group`, `send_text_message`, `send_chat_action`, `set_message_reaction`.
- **concurrency.rs** — `ConcurrencyLimiter` using DashSet<ChatId> with RAII `LockGuard`. One request per user at a time.
- **validator.rs** — Pre-download checks: max 30min duration, 500MB filesize, 5 video playlist items, 10 image gallery items.
- **test_utils.rs** — `create_test_metadata()` factory for tests.

### Key Design Patterns
- **Trait-based dependency injection**: `Downloader` and `TelegramApi` traits are `#[automock]`ed (mockall crate) for unit testing. Handler tests use `MockDownloader` + `MockTelegramApi`.
- **RAII file cleanup**: `FileCleanupGuard` in handler.rs spawns a background tokio task to delete downloaded files on drop.
- **Per-user concurrency limiting**: DashSet-based lock prevents concurrent downloads per chat.

### Infrastructure (docker-compose.yml)
Three services: **bot** (the Rust binary), **cloudflared** (Cloudflare tunnel), **telegram-bot-api** (local API server for >50MB uploads). The override file adds local build and a `test-runner` service.

### Custom yt-dlp Fork
Built from source (https://github.com/Enucatl/yt-dlp.git) as a Python zipapp in the Docker multi-stage build. Requires Python 3.14 at runtime.

## Environment Variables

Required in `.env`: `TELOXIDE_TOKEN`, `WEBHOOK_URL`, `TELEGRAM_API_ID`, `TELEGRAM_API_HASH`, `TUNNEL_TOKEN`. Optional: `EXECUTION_ENVIRONMENT` (gcp/local/homelab), `RUST_LOG`, `PORT` (default 8080).

## Rust Edition

Uses Rust edition 2024.
