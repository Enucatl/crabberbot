# CrabberBot 🦀

[![GitHub Actions CI/CD](https://github.com/Enucatl/crabberbot/actions/workflows/deploy.yml/badge.svg)](https://github.com/Enucatl/crabberbot/actions/workflows/deploy.yml)
[![Made with Rust](https://img.shields.io/badge/made%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

CrabberBot is your friendly and powerful Telegram bot for downloading videos, photos, and galleries from a wide variety of websites. Built with Rust for performance and reliability, it leverages the power of `yt-dlp` to offer extensive platform support.

**➡️ [Try CrabberBot now!](https://t.me/crabberbot) ⬅️**

---

## ✨ Features

-   **Wide Platform Support**: Downloads media from YouTube (including Shorts), Instagram (Posts, Reels, Stories), TikTok, Twitter/X, and virtually any other site supported by `yt-dlp`.
-   **Handles Galleries & Playlists**: Seamlessly processes multi-image/video posts (like Instagram galleries) and sends them as a clean media group. It can also handle short video playlists.
-   **Intelligent Captioning**: Automatically generates a concise and informative caption, including a link to the original source and the uploader's name, while respecting Telegram's character limits (1024).
-   **Pre-Download Validation**: Protects against abuse and saves time by checking media duration, file size, and playlist length *before* downloading. You'll be notified if the media is too large or long.
-   **High-Performance & Concurrent**: Built in Rust with an asynchronous architecture, it can handle multiple requests efficiently. It also includes a per-user lock to prevent you from accidentally spamming requests.
-   **Large File Support**: Utilizes a local Telegram Bot API server to bypass the standard 50MB upload limit, allowing for larger video downloads.
-   **Privacy & Automatic Cleanup**: Ensures link identifiers are removed before downloading and sharing, and temporary media files are securely and automatically deleted from the server after being sent.

## 🚀 How to Use

Using CrabberBot is as simple as it gets:

1.  **Open a chat with the bot**: [@crabberbot](https://t.me/crabberbot)
2.  **Send a link**: Paste the URL of the video, photo, or post you want to download.
3.  **Receive your media**: The bot will process the link, download the content, and send it back to you directly in the chat!

### Supported Commands

-   `/start` - Displays a welcome message and a guide on how to use the bot.
-   `/version` - Shows the current running version of the bot.

---

## 🛠️ Self-Hosting Guide for Developers

You can easily run your own instance of CrabberBot using Docker and Docker Compose. This is for users who want to run a private instance or contribute to development.

### Prerequisites

-   **Docker** and **Docker Compose**
-   **Telegram Bot Token**: Get one from [@BotFather](https://t.me/BotFather) on Telegram.
-   **Telegram API Credentials**: Get `api_id` and `api_hash` from [my.telegram.org](https://my.telegram.org). This is for the local API server to handle large files.
-   **Cloudflare Account** and **Tunnel Token**: To expose your local bot to the internet for Telegram Webhooks. You can get a token from the Cloudflare Zero Trust dashboard. ### 1. Clone the Repository
```bash
git clone https://github.com/Enucatl/crabberbot.git
cd crabberbot
```

### 2. Configure Environment Variables

Create a `.env` file in the root of the project. You can copy `docker-compose.override.yml` for local build arguments, but you'll need to create the main `.env` for secrets used by `docker-compose.yml`.

Example `.env` file:
```dotenv
# Your Telegram Bot Token from @BotFather
TELOXIDE_TOKEN=123456:ABC-DEF1234567890

# The public URL for Telegram webhooks (provided by your Cloudflare Tunnel)
# Example: https://your-tunnel-name.trycloudflare.com
WEBHOOK_URL=https://your-tunnel-url.trycloudflare.com

# Your Telegram App credentials from my.telegram.org for the local API server
TELEGRAM_API_ID=12345678
TELEGRAM_API_HASH=your_api_hash_here

# Your Cloudflare Tunnel Token
TUNNEL_TOKEN=your_tunnel_token_here

# Optional: Set verbosity for the local Telegram API server (0-4)
TELEGRAM_VERBOSITY=1
```

### 3. Run the Stack

Build and test locally:
```bash
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') cargo build
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') cargo test

With your `.env` file configured, start the entire application stack with a single command:
```
```bash
docker-compose up -d
```

This will:
-   Pull the pre-built images for the bot, API server, and tunnel.
-   Start all three services.

Your bot instance is now live!

### 4. Local Development & Testing

The provided `docker-compose.override.yml` makes local development easy.

-   **To build and run a local version of the bot (instead of pulling from GHCR)**:
    ```bash
    CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --env-file .env.test up --build
    ```
-   **To run the test suite inside a Docker container**:
    ```bash
    # This uses the 'test' profile defined in the override file
    CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --profile test run --build --rm test-runner
    ```

## 🏗️ Technical Architecture

The bot is composed of several services that work together, all managed by Docker Compose.

1.  **`crabberbot` (Rust Application)**: The core of the bot. It's written in Rust using the `teloxide` framework. It handles incoming messages, parses URLs, interacts with `yt-dlp`, validates media, and sends files back to the user.
2.  **`yt-dlp`**: The workhorse for downloading. It's built from source within the `Dockerfile` to ensure the latest features and fixes. The bot executes `yt-dlp` as a command-line process.
3.  **`telegram-bot-api` (Local Server)**: A local instance of the Telegram Bot API. **This is crucial for uploading files larger than 50MB**. By running our own API server, we bypass the standard file size limit imposed on bots using Telegram's public API.
4.  **`cloudflared` (Webhook Tunnel)**: Creates a secure tunnel from a public Cloudflare URL to the bot running on your local machine. This allows Telegram's servers to send webhook updates to the bot without you needing to configure firewalls or port forwarding.

## ❤️ Contributing

Contributions are welcome! If you have a feature request, bug report, or pull request, please feel free to open an issue or submit a PR.

1.  Fork the repository.
2.  Create a new feature branch (`git checkout -b feature/your-feature`).
3.  Commit your changes (`git commit -am 'Add some feature'`).
4.  Push to the branch (`git push origin feature/your-feature`).
5.  Open a new Pull Request.

## 📄 License

This project is licensed under the GNU General Public License v3.0 - see the [LICENSE](LICENSE) file for details.

