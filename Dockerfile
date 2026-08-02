FROM rust:1-slim-trixie AS builder

RUN apt update && apt install -y build-essential git libssl-dev make pkg-config python3 zip \
    && rm -rf /var/lib/apt/lists/*

ARG YT_DLP_REPO_URL="https://github.com/Enucatl/yt-dlp.git"
ARG YT_DLP_COMMIT_HASH="master"
RUN set -eux; git init /tmp/yt-dlp; cd /tmp/yt-dlp; git remote add origin "${YT_DLP_REPO_URL}"; \
    git fetch --depth 1 origin "${YT_DLP_COMMIT_HASH}"; git checkout --detach FETCH_HEAD; \
    make yt-dlp; mv yt-dlp /usr/local/bin/yt-dlp; rm -rf /tmp/yt-dlp

WORKDIR /usr/src/crabberbot
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src target/release/deps/crabberbot*
COPY src ./src
COPY build.rs ./build.rs
COPY migrations ./migrations
ARG CARGO_PACKAGE_VERSION
ENV CARGO_PACKAGE_VERSION=${CARGO_PACKAGE_VERSION}
RUN cargo build --release && cargo test --no-run

FROM debian:trixie-slim AS app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home --shell /bin/bash appuser \
    && mkdir /downloads /downloader && chown appuser:appuser /downloads /downloader
USER appuser
WORKDIR /home/appuser
COPY --from=builder /usr/src/crabberbot/target/release/crabberbot .
EXPOSE 8080
VOLUME ["/downloads", "/downloader"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["curl", "-fsS", "http://127.0.0.1:8080/healthz"]
CMD ["./crabberbot"]

FROM python:3.14-slim-trixie AS downloader-worker
RUN DEBIAN_FRONTEND=noninteractive apt-get update && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir "curl_cffi>=0.15,<0.16" requests brotli \
    && useradd --uid 1000 --create-home --shell /bin/bash appuser \
    && mkdir /downloads /downloader && chown appuser:appuser /downloads /downloader
USER appuser
WORKDIR /home/appuser
COPY --from=builder /usr/src/crabberbot/target/release/downloader-worker .
COPY --from=builder /usr/local/bin/yt-dlp /usr/local/bin/
VOLUME ["/downloads", "/downloader"]
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=3 CMD ["test", "-S", "/downloader/downloader.sock"]
CMD ["./downloader-worker"]
