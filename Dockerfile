# ---- Build Stage: Compile the application and build yt-dlp ----
FROM rust:1-slim-bullseye as builder

# Install system dependencies for Rust compilation and for building yt-dlp
# We now need 'make' in addition to 'git'.
RUN apt update && apt install -y \
    build-essential \
    git \
    libssl-dev \
    make \
    pkg-config \
    python3 \
    zip \
    && rm -rf /var/lib/apt/lists/*

# Build yt-dlp from source.
# We do a shallow clone, build the binary, move it to a standard location,
# and clean up the source code to keep this layer smaller.
#
# --- IMPORTANT ---
# Change the URL below to point to your fork of yt-dlp.
ARG YT_DLP_REPO_URL="https://github.com/Enucatl/yt-dlp.git"
ARG YT_DLP_COMMIT_HASH="threads"
RUN git clone --depth 1 --branch master "${YT_DLP_REPO_URL}" /tmp/yt-dlp && \
    cd /tmp/yt-dlp && \
    git fetch --depth 1 origin "${YT_DLP_COMMIT_HASH}" && \
    git checkout FETCH_HEAD && \
    make yt-dlp && \
    mv yt-dlp /usr/local/bin/yt-dlp && \
    rm -rf /tmp/yt-dlp

WORKDIR /usr/src/crabberbot

# Copy manifests and pre-build dependencies to leverage Docker layer caching.
COPY Cargo.toml ./
# Create a dummy project to build only dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
# Clean up dummy files
RUN rm -rf src target/release/deps/crabberbot*

# Copy the actual source code and build files
COPY src ./src
COPY build.rs ./build.rs

ARG CARGO_PACKAGE_VERSION="unknown"

# ---- Test Stage: Uses cached dependencies to run tests ----
FROM builder as tester

WORKDIR /usr/src/crabberbot

# If you have an integration tests directory, copy it too
# COPY tests ./tests

RUN CARGO_PACKAGE_VERSION=${CARGO_PACKAGE_VERSION} cargo test -- --nocapture

# ---- Main Build Continuation: Build the final application binary ----
FROM builder as final_builder

WORKDIR /usr/src/crabberbot

# Build the application
RUN CARGO_PACKAGE_VERSION=${CARGO_PACKAGE_VERSION} cargo build --release

# ---- Runtime Stage: Create the final, smaller image ----
FROM debian:bullseye-slim as runtime

# The yt-dlp binary is a zipapp that requires the python3 interpreter to run.
# We don't need git, make, or pip in the final image.
RUN apt-get update && apt-get install -y \
    ca-certificates \
    python3 \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user for security best practices
RUN useradd --create-home --shell /bin/bash appuser
USER appuser
WORKDIR /home/appuser

# Copy the compiled Rust binary from the builder stage
COPY --from=final_builder /usr/src/crabberbot/target/release/crabberbot .

# Copy the yt-dlp binary that was built in the builder stage
COPY --from=builder /usr/local/bin/yt-dlp /usr/local/bin/

# Expose the port the bot listens on for webhooks
EXPOSE 8080

# Set the command to run the bot
CMD ["./crabberbot"]
