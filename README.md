# CrabberBot

@crabberbot is a video downloader bot for telegram.

It can download videos and photos from various platforms like Instagram, TikTok, YouTube Shorts, and many more!

<b>How to use me</b>
To download media, simply send the URL of the media you want to download.
Example: <code>https://www.youtube.com/shorts/tPEE9ZwTmy0</code>

# Build, test and run
```
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') cargo build
```

```
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --profile test run --build --rm test-runner

CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --env-file .env up --build
```
