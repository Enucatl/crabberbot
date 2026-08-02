# CrabberBot 🦀

[![GitHub Actions CI/CD](https://github.com/Enucatl/crabberbot/actions/workflows/deploy.yml/badge.svg)](https://github.com/Enucatl/crabberbot/actions/workflows/deploy.yml)
[![Image](https://img.shields.io/github/v/tag/Enucatl/crabberbot?label=ghcr.io%2Fenucatl%2Fcrabberbot)](https://github.com/Enucatl/crabberbot/pkgs/container/crabberbot)
[![Trivy scan](https://img.shields.io/github/actions/workflow/status/Enucatl/crabberbot/deploy.yml?label=trivy%20scan)](https://github.com/Enucatl/crabberbot/actions/workflows/deploy.yml)
[![Made with Rust](https://img.shields.io/badge/made%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

CrabberBot is a Telegram bot that downloads videos, photos, and galleries from sites supported by [`yt-dlp`](https://github.com/yt-dlp/yt-dlp).

**[Try CrabberBot](https://t.me/crabberbot)**

## Features

- Downloads supported videos, photos, galleries, and short playlists.
- Validates duration, file size, and playlist length before downloading.
- Caches Telegram uploads and audio so repeated requests avoid duplicate work.
- Offers optional audio extraction, transcription, and summarization through Telegram Stars subscriptions and top-ups.
- Uses a local Telegram Bot API server for uploads larger than the public Bot API limit.

## Commands

- `/start` — usage guide
- `/version` — running version
- `/subscribe` — plans and top-ups
- `/terms` — Terms of Service
- `/support <message>` — contact support
- `/refundme` — request a refund for the most recent eligible purchase

## Architecture

Docker Compose runs the following services:

| Service | Responsibility | Network access |
| --- | --- | --- |
| `crabberbot` | Rust webhook application, PostgreSQL access, Telegram uploads, and premium APIs | `backend`, `internet` |
| `downloader-worker` | `yt-dlp`, `ffmpeg`, and `ffprobe` behind a Unix socket | `downloader` only |
| `egress-proxy` | The worker's sole Internet path | `downloader`, `internet` |
| `telegram-bot-api` | Local Telegram Bot API server for large uploads | `backend`, `internet` |
| `cloudflared` | Cloudflare Tunnel for Telegram webhooks | `backend`, `internet` |
| `postgres` | Media cache, requests, billing, and callback state | `backend` only |

The app and downloader communicate through `/downloader/downloader.sock`; the app image contains no `yt-dlp` or `ffmpeg` fallback. The worker passes all `yt-dlp` traffic through `egress-proxy`, so it cannot directly reach either the backend network or the Internet. The proxy is intentionally deployed as part of the stack, not as an optional application setting.

The app exposes `GET /healthz`, returning `204` only when PostgreSQL accepts `SELECT 1`; Docker uses it as the application health check.

## Premium features

After a supported video download, the bot can offer:

- **Extract Audio** — cached MP3 produced by the worker.
- **Transcribe** — Deepgram transcription of the cached audio.
- **Summarize** — Deepgram transcription followed by Gemini summarization.

The public products are Basic (50 Stars/month, 60 AI Video Minutes), Pro (150 Stars/month, 200 AI Video Minutes plus unlimited audio extraction), and a 60-minute top-up (50 Stars). AI Video Minutes are charged by video duration. Top-ups work without a subscription and expire 365 days after the latest top-up purchase. The bot's `/terms` command is the authoritative customer-facing policy.

Premium callback contexts are bound to the Telegram user who requested the download. Quota reservations, payment fulfilment, and refunds are persisted in PostgreSQL to prevent duplicate charges or delivery.

## Self-hosting

### Prerequisites

- Docker Compose
- Telegram bot token from [@BotFather](https://t.me/BotFather)
- Telegram API ID and hash from [my.telegram.org](https://my.telegram.org)
- Cloudflare Tunnel token and public webhook URL

Clone the repository:

```bash
git clone https://github.com/Enucatl/crabberbot.git
cd crabberbot
```

Create these secret files:

- `secrets/telegram_api_id`
- `secrets/telegram_api_hash`
- `secrets/tunnel_token`

Then create `.env`:

```dotenv
TELOXIDE_TOKEN=123456:ABC-DEF1234567890
WEBHOOK_URL=https://your-tunnel.example.com
POSTGRES_PASSWORD=change-me
# Optional
TELEGRAM_VERBOSITY=1
DEEPGRAM_API_KEY=
GEMINI_API_KEY=
OWNER_CHAT_ID=
MAX_YT_DLP_SESSIONS=4
```

Compose mounts the Telegram API ID and hash files under `/run/secrets`; they aren't exposed in the Telegram Bot API container environment.

`DEEPGRAM_API_KEY` and `GEMINI_API_KEY` are required only for transcription and summarization. `OWNER_CHAT_ID` enables owner-only grants, support replies, and refunds.

Start the stack:

```bash
docker compose up -d
```

### Development

Run formatting and the test suite. SQLx storage tests need a disposable PostgreSQL database:

```bash
cargo fmt --all --check
DATABASE_URL=postgres://postgres:postgres@localhost:5432/crabberbot cargo test --verbose
```

For a local Compose build, use the supplied override and test environment:

```bash
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/./') \
  docker compose --env-file .env.test up --build
```

The worker builds `yt-dlp` from source. To refresh that Docker layer only when upstream changes, provide the current commit hash:

```bash
YT_DLP_COMMIT_HASH=$(git ls-remote https://github.com/Enucatl/yt-dlp.git refs/heads/master | cut -f1) \
  CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/./') \
  docker compose build downloader-worker
```

## Security

The Compose services extend the shared [docker-compose-security-baseline](https://github.com/Enucatl/docker-compose-security-baseline) for non-root execution, reduced capabilities, no-new-privileges, and memory, swap, and PID limits. See [SECURITY.md](SECURITY.md) for dependency-audit exceptions.

## Contributing

Contributions are welcome. Please format the code and run the test suite before opening a pull request.
